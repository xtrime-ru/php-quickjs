use std::{env, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=src/notification.c");
    println!("cargo:rerun-if-env-changed=PHP_CONFIG");
    let php_config = env::var("PHP_CONFIG").unwrap_or_else(|_| "php-config".into());
    let output = Command::new(php_config)
        .arg("--includes")
        .output()
        .expect("php-config --includes");
    assert!(output.status.success(), "php-config --includes failed");
    let mut build = cc::Build::new();
    build.file("src/notification.c");
    for flag in String::from_utf8(output.stdout)
        .expect("PHP include paths")
        .split_whitespace()
    {
        build.flag(flag);
    }
    build.compile("quickjs_notification");
}
