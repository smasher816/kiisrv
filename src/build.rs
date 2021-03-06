use crate::kll::KllConfig;

use crate::kll::*;
use shared_child::SharedChild;
use std::process::Command;

#[derive(Debug)]
pub struct BuildInfo {
    pub name: String,
    pub variant: String,
    pub layout: String,
    pub build_script: String,
    pub default_map: Vec<String>,
    pub partial_maps: Vec<String>,
    pub split_keyboard: bool,
}

pub fn configure_build(config: &KllConfig, layers: Vec<String>) -> BuildInfo {
    let name = config.header.name.replace(" ", "_"); //sanitize
    let variant = config
        .header
        .variant
        .clone()
        .unwrap_or("".to_string())
        .replace(" ", "_");
    let layout = config
        .header
        .layout
        .clone()
        .replace(" ", "_");

    let build_script = match name.to_lowercase().as_ref() {
        "md1" => "infinity.bash",
        "md1.1" => "infinity_led.bash",
        "mdergo1" => "ergodox.bash",
        "whitefox" => "whitefox.bash",
        "ktype" => "k-type.bash",
        "kira" => "kira.bash",
        _ => panic!("Unknown keyboard {}", name),
    }
    .to_string();

    let split_keyboard = match name.to_lowercase().as_ref() {
        "mdergo1" => true,
        _ => false,
    };

    let extra_map = match name.to_lowercase().as_ref() {
        "mdergo1" => vec![
            "stdFuncMap".to_string(),
            "infinity_ergodox/lcdFuncMap".to_string(),
        ],
        _ => vec!["stdFuncMap".to_string()],
    };

    let mut layers = layers.iter();
    let base_layer = kll_filename(layers.next().unwrap());

    let default_map = {
        let mut layer = extra_map.clone();
        layer.push(base_layer.to_string());
        layer
    };

    let partial_maps = layers
        .map(|l| {
            let mut layer = extra_map.clone();
            layer.push(l.to_string());
            kll_layer(layer)
        })
        .collect::<Vec<_>>();

    BuildInfo {
        name,
        variant,
        layout,
        build_script,
        default_map,
        partial_maps,
        split_keyboard,
    }
}

pub fn start_build(
    container: String,
    config: BuildInfo,
    kll_dir: String,
    output_file: String,
) -> SharedChild {
    /*let mut sleep = Command::new("sleep");
    sleep.args(&["1"]);
    let process = SharedChild::spawn(&mut sleep).expect("Failed to execute!");*/

    let mut args = vec![
        "run".to_string(),
        "--rm".to_string(),
        "-T".to_string(),
        "-e".to_string(),
        format!("DefaultMapOverride={}", kll_layer(config.default_map)),
        "-e".to_string(),
        format!(
            "PartialMapsExpandedOverride={}",
            kll_list(config.partial_maps)
        ),
        "-e".to_string(),
        format!("Layout={}", config.variant),
    ];

    if config.split_keyboard {
        args.push("-e".to_string());
        args.push("SPLIT_KEYBOARD=1".to_string());
    }

    args.extend_from_slice(&[container, config.build_script, kll_dir, output_file]);

    let mut compile = Command::new("docker-compose");
    compile.args(&args);
    let process = SharedChild::spawn(&mut compile).expect("Failed to execute!");

    println!(" >> Created PID: {}", process.id());
    return process;
}

pub fn list_containers() -> Vec<String> {
    let result = Command::new("docker-compose")
        .args(&["config", "--services"])
        .output()
        .expect("Failed!");
    let out = String::from_utf8_lossy(&result.stdout);
    out.lines()
        .filter(|s| !s.contains("template"))
        .map(|s| s.to_string())
        .collect()
}

pub fn get_builds(service: &str) -> String {
    let result = Command::new("docker-compose")
        .args(&[
            "run",
            "--rm",
            "--entrypoint",
            "/usr/bin/find",
            service,
            "/mnt/builds",
            "-printf",
            "%P\n",
        ])
        .output()
        .expect("Failed!");
    String::from_utf8_lossy(&result.stdout).to_string()
}

fn old_builds(service: &str) {
    let status = Command::new("docker-compose")
        .args(&[
            "run",
            "--rm",
            "--entrypoint",
            "/usr/bin/find",
            service,
            "/mnt/builds",
            //"-mtime", "+1",
            "-depth",
            "-mmin",
            "+5",
            "-print",
        ])
        .status()
        .expect("Failed!");

    if status.success() {
        println!("Success!");
    } else {
        println!("Failed.");
    }
}
