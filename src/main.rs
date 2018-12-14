mod build;
mod kll;

use crate::build::*;
use crate::kll::*;

use std::collections::hash_map::{DefaultHasher, HashMap};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Arc;

use bodyparser;
use iron::prelude::*;
use iron::{headers, modifiers::Header, status, typemap::Key};
use logger::Logger;
use mount::Mount;
use persistent::{Read, Write};
use staticfile::Static;

use chrono::prelude::*;
use rusqlite::{types::ToSql, Connection};

use serde_derive::{Deserialize, Serialize};
use serde_json;
use shared_child::SharedChild;

const API_HOST: &str = "0.0.0.0:3000";
const MAX_BODY_LENGTH: usize = 1024 * 1024 * 10;
const BUILD_ROUTE: &str = "./tmp";

const LAYOUT_DIR: &str = "./layouts";
const BUILD_DIR: &str = "./tmp_builds";
const CONFIG_DIR: &str = "./tmp_config";

const DB_FILE: &str = "./stats.db";
const DB_SCHEMA: &str = include_str!("../db-schema.sqlite");

#[derive(Clone, Deserialize)]
pub struct BuildRequest {
    pub config: KllConfig,
    pub env: String,
}

#[derive(Clone, Serialize)]
pub struct BuildResult {
    pub filename: String,
    pub success: bool,
}

#[derive(Copy, Clone)]
pub struct JobQueue;
impl Key for JobQueue {
    type Value = HashMap<String, Option<Arc<SharedChild>>>;
}

#[derive(Copy, Clone)]
pub struct Database;
impl Key for Database {
    type Value = rusqlite::Connection;
}

#[derive(Debug)]
struct RequestLog {
    id: i32,
    uid: Option<i32>,
    ip_addr: String,
    os: String,
    web: bool,
    serial: Option<i32>,
    hash: String,
    board: String,
    variant: String,
    layers: i32,
    container: String,
    success: bool,
    request_time: DateTime<Utc>,
    build_duration: Option<i32>,
}
impl RequestLog {
    fn from_row(row: &rusqlite::Row) -> Self {
        RequestLog {
            id: row.get(0),
            uid: row.get(1),
            ip_addr: row.get(2),
            os: row.get(3),
            web: row.get(4),
            serial: row.get(5),
            hash: row.get(6),
            board: row.get(7),
            variant: row.get(8),
            layers: row.get(9),
            container: row.get(10),
            success: row.get(11),
            request_time: row.get(12),
            build_duration: row.get(13),
        }
    }
}

fn build_request(req: &mut Request<'_, '_>) -> IronResult<Response> {
    //println!("Headers: {:?}", req.headers);
    if let Ok(Some(body)) = req.get::<bodyparser::Struct<BuildRequest>>() {
        let ip = req.remote_addr.ip();
        let user_agent = req
            .headers
            .get::<headers::UserAgent>()
            .unwrap_or(&iron::headers::UserAgent("".to_owned()))
            .to_string();

        let os = {
            let ua = user_agent.to_lowercase();
            if ua.contains("windows") {
                "Windows"
            } else if ua.contains("mac") {
                "Mac"
            } else if ua.contains("linux") || ua.contains("x11") {
                "Linux"
            } else {
                "Unknown"
            }
        }
        .to_string();

        let is_desktop_configurator = user_agent.to_lowercase().contains("electron");
        println!("IP: {:?}", ip);
        println!("OS: {:?}", os);
        println!("WEB: {:?}", !is_desktop_configurator);

        let request_time: DateTime<Utc> = Utc::now();

        let config = body.config;
        let container = match body.env.as_ref() {
            "lts" => "controller-050",
            "latest" | _ => "controller-051",
        }
        .to_string();

        let config_str = serde_json::to_string(&config).unwrap();

        let hash = {
            let mut hasher = DefaultHasher::new();
            container.hash(&mut hasher);
            config_str.hash(&mut hasher);
            let h = hasher.finish();
            format!("{:x}", h)
        };
        println!("Received request: {}", hash);

        let job: Option<Arc<SharedChild>> = {
            let mutex = req.get::<Write<JobQueue>>().unwrap();
            let mut queue = mutex.lock().unwrap();

            if let Some(job) = (*queue).get(&hash) {
                println!(" > Existing task");
                job.clone()
            } else {
                println!(" > Starting new build");

                let config_dir = format!("{}/{}", CONFIG_DIR, hash);
                fs::create_dir_all(&config_dir).unwrap();

                let mut layers: Vec<String> = Vec::new();
                let files = generate_kll(&config, body.env == "lts");
                for file in files {
                    let filename = format!("{}/{}", config_dir, file.name);
                    fs::write(&filename, file.content).unwrap();
                    layers.push(format!("{}", filename));
                }

                let info = configure_build(&config, layers);
                let output_file = format!("{}-{}-{}.zip", info.name, info.variant, hash);
                println!("{:?}", info);

                let config_file = format!("{}/{}-{}.json", config_dir, info.name, info.variant);
                fs::write(&config_file, &config_str).unwrap();

                let process = start_build(container.clone(), info, hash.clone(), output_file);
                let arc = Arc::new(process);
                (*queue).insert(hash.clone(), Some(arc.clone()));
                Some(arc)
            }

            // drop lock
        };

        let info = configure_build(&config, vec!["".to_string()]);
        let output_file = format!("{}-{}-{}.zip", info.name, info.variant, hash);

        let (success, duration) = match job {
            Some(arc) => {
                let process = arc.clone();
                println!(" > Waiting for task to finish {}", process.id());
                let exit_status = process.wait().unwrap();
                println!(" > Done");

                {
                    let rwlock = req.get::<Write<JobQueue>>().unwrap();
                    let mut queue = rwlock.lock().unwrap();
                    let job = (*queue).get_mut(&hash).unwrap();
                    *job = None;
                    // drop lock
                }

                let duration = Some(Utc::now().signed_duration_since(request_time));
                let success = exit_status.success();
                (success, duration)
            }
            None => {
                println!(" > Job already in finished {}. Updating time.", hash);
                (true, None)
            }
        };

        let build_duration = match duration {
            Some(t) => Some(t.num_milliseconds()),
            None => None,
        };
        println!(
            "Started at: {:?}, Duration: {:?}",
            request_time, build_duration
        );

        let layers = vec![""];
        let args: &[&ToSql] = &[
            &(ip.to_string()),
            &os,
            &!is_desktop_configurator,
            &hash,
            &info.name,
            &info.variant,
            &(layers.len() as u32),
            &container,
            &success,
            &request_time,
            &build_duration,
        ];

        {
            let mutex = req.get::<Write<Database>>().unwrap();
            let db = mutex.lock().unwrap();
            // TODO: uid, serial
            (*db).execute("INSERT INTO Requests (ip_addr, os, web, hash, board, variant, layers, container, success, request_time, build_duration)
                  VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)", args).unwrap();
        }

        if success {
            let result = BuildResult {
                filename: format!("{}/{}", BUILD_ROUTE, output_file),
                success: true,
            };

            return Ok(Response::with((
                status::Ok,
                Header(headers::ContentType::json()),
                serde_json::to_string(&result).unwrap(),
            )));
        } else {
            return Ok(Response::with((
                status::InternalServerError,
                Header(headers::ContentType::json()),
                "{ error: \"build failed\" }",
            )));
        }
    } else if let Err(err) = req.get::<bodyparser::Struct<BuildRequest>>() {
        println!("Parse error: {:?}", err);
        use bodyparser::BodyErrorCause::JsonError;
        let s = if let JsonError(e) = err.cause {
            println!("e: {:?}", e);
            e.to_string()
        } else {
            err.detail
        };

        return Ok(Response::with((
            status::BadRequest,
            Header(headers::ContentType::json()),
            format!("{{ \"error\": \"{}\" }}", s),
        )));
    }

    return Ok(Response::with((
        status::BadRequest,
        Header(headers::ContentType::json()),
        "{ \"error\": \"bad request\" }",
    )));
}

fn stats(req: &mut Request<'_, '_>) -> IronResult<Response> {
    let mutex = req.get::<Write<Database>>().unwrap();
    let db = mutex.lock().unwrap();
    let args: &[&ToSql] = &[];

    let mut result = String::new();
    let mut total_layers: usize = 0;
    let mut total_buildtime = 0;
    let mut os_counts: HashMap<String, usize> = HashMap::new();
    let mut platform_counts: HashMap<String, usize> = HashMap::new();
    let mut keyboard_counts: HashMap<String, usize> = HashMap::new();
    let mut container_counts: HashMap<String, usize> = HashMap::new();
    let mut hashes: Vec<String> = Vec::new();
    let mut users: Vec<String> = Vec::new();

    let mut stmt = (*db).prepare("SELECT * FROM Requests").unwrap();
    let rows = stmt
        .query_map(args, |row| RequestLog::from_row(row))
        .unwrap();
    for row in rows {
        let request = row.unwrap();
        println!("req: {:?}", request);

        let counter = os_counts.entry(request.os).or_insert(0);
        *counter += 1;

        let platform = match request.web {
            true => "Web",
            false => "Desktop",
        }
        .to_string();
        let counter = platform_counts.entry(platform).or_insert(0);
        *counter += 1;

        let keyboard = format!("{}-{}", request.board, request.variant);
        let counter = keyboard_counts.entry(keyboard).or_insert(0);
        *counter += 1;

        let counter = container_counts.entry(request.container).or_insert(0);
        *counter += 1;

        total_layers += request.layers as usize;
        total_buildtime += request.build_duration.unwrap_or(0) as i32;

        hashes.push(request.hash);
        users.push(request.ip_addr); //requst.uid
    }

    let total_builds = hashes.len();
    hashes.sort();
    hashes.dedup();
    let unique_builds = hashes.len();

    users.sort();
    users.dedup();
    let unique_users = users.len();

    let cache_ratio = match unique_builds {
        0 => 0.,
        _ => (total_builds as f32) / (unique_builds as f32),
    };

    let user_ratio = match unique_builds {
        0 => 0.,
        _ => (total_builds as f32) / (unique_users as f32),
    };

    let layers_ratio = match unique_builds {
        0 => 0.,
        _ => (total_layers as f32) / (total_builds as f32),
    };

    let build_time = match unique_builds {
        0 => 0,
        _ => total_buildtime / (unique_builds as i32),
    };

    result += &format!("Builds: {} ({} unique)\n", total_builds, unique_builds);
    result += &format!("Cache ratio: {:.1}\n", cache_ratio);
    result += &format!("Avg time: {:.3} s\n\n", (build_time as f32) / 1000.0);
    result += &format!("Users: {} unique\n", unique_users);
    result += &format!("Avg builds per user: {:.1}\n", user_ratio);
    result += &format!("Average number of layers: {}\n\n", layers_ratio);
    result += &format!("OS Counts: {:#?}\n", os_counts);
    result += &format!("Platform Counts: {:#?}\n", platform_counts);
    result += &format!("Keyboard Counts: {:#?}\n", keyboard_counts);
    result += &format!("Version Counts: {:#?}\n\n", container_counts);

    return Ok(Response::with((status::Ok, result)));
}

fn main() {
    pretty_env_logger::init();

    /*let status = Command::new("docker-compose")
        .args(&["-f", "docker-compose.yml", "up", "-d", "--no-recreate"])
        .status()
        .expect("Failed!");

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }*/

    let queue: HashMap<String, Option<Arc<SharedChild>>> = HashMap::new();
    let args: &[&ToSql] = &[];
    let db = Connection::open(Path::new(DB_FILE)).unwrap();
    db.execute(DB_SCHEMA, args).unwrap();

    /*println!("\nExisting builds: ");
    let builds = get_builds("controller-051");
    for build in builds.lines().skip(1) {
        println!(" - {}", build);
        queue.insert(build.to_string(), None);
    }

    println!("\nBuilds to purge: ");
    old_builds("controller-050");
    println!("");*/

    println!("\nAvailable build containers:");
    println!("{:?}", list_containers());

    let (logger_before, logger_after) = Logger::new(None);

    let mut mount = Mount::new();
    mount.mount("/layouts/", Static::new(Path::new(LAYOUT_DIR)));
    mount.mount("/tmp/", Static::new(Path::new(BUILD_DIR)));
    mount.mount("/stats", stats);
    mount.mount("/", build_request);

    println!("\nBuild dispatcher starting.\nListening on {}", API_HOST);
    let mut chain = Chain::new(mount);
    chain.link_before(Write::<JobQueue>::one(queue));
    chain.link_before(Write::<Database>::one(db));
    chain.link_before(Read::<bodyparser::MaxBodyLength>::one(MAX_BODY_LENGTH));
    chain.link_before(logger_before);
    chain.link_after(logger_after);
    Iron::new(chain).http(API_HOST).unwrap();
}
