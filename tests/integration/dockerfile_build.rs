//! Integration tests for Dockerfile building.
//!
//! Tests the full pipeline from Dockerfile parsing through
//! command execution to image construction.

use dockerfile_parser::parse_dockerfile;
use kaniko_core::multistage_builder::MultiStageBuilder;
use oci_image::config::ContainerConfig;
use oci_image::mutate::MutableImage;
use std::path::PathBuf;

/// Helper: parse a Dockerfile and return stages.
fn parse_stages(dockerfile: &str) -> dockerfile_parser::Result<Vec<dockerfile_parser::Stage>> {
    parse_dockerfile(dockerfile)
}

#[tokio::test]
async fn test_simple_from_env() {
    let dockerfile = r#"
FROM scratch
ENV FOO=bar
"#;
    let stages = parse_stages(dockerfile).expect("Failed to parse Dockerfile");
    assert_eq!(stages.len(), 1);
    let stage = &stages[0];
    assert_eq!(stage.image, "scratch");
    assert!(!stage.instructions.is_empty());
}

#[tokio::test]
async fn test_multi_stage_parse() {
    let dockerfile = r#"
FROM golang:1.24 AS builder
RUN go build -o /app

FROM alpine:3.18
COPY --from=builder /app /app
"#;
    let stages = parse_stages(dockerfile).expect("Failed to parse Dockerfile");
    assert_eq!(stages.len(), 2);
    assert_eq!(stages[0].alias, Some("builder".to_string()));
    assert_eq!(stages[1].alias, None);
}

#[tokio::test]
async fn test_multistage_build_order() {
    let dockerfile = r#"
FROM alpine:3.18 AS base
RUN echo base

FROM golang:1.24 AS builder
COPY --from=base /etc/passwd /tmp/
RUN go build -o /app

FROM alpine:3.18
COPY --from=builder /app /app
"#;
    let stages = parse_stages(dockerfile).expect("Failed to parse Dockerfile");
    let builder = MultiStageBuilder::new(stages, PathBuf::from("/tmp"));

    let order = builder.determine_build_order().expect("Failed to determine build order");
    assert_eq!(order.len(), 3);

    let base_pos = order.iter().position(|&x| x == 0).unwrap();
    let builder_pos = order.iter().position(|&x| x == 1).unwrap();
    assert!(base_pos < builder_pos, "base should be built before builder");

    let final_pos = order.iter().position(|&x| x == 2).unwrap();
    assert!(builder_pos < final_pos, "builder should be built before final");
}

#[tokio::test]
async fn test_image_config_operations() {
    let mut image = MutableImage::empty();

    let layer = oci_image::layer::Layer::empty().expect("Failed to create empty layer");
    image = oci_image::mutate::append_layer(image, layer).expect("Failed to append layer");
    assert_eq!(image.layer_count(), 1);

    image = oci_image::mutate::set_env(image, "APP_ENV", "production").expect("Failed to set env");
    assert_eq!(image.config.config.get_env("APP_ENV"), Some("production".to_string()));

    image = oci_image::mutate::set_entrypoint(image, vec!["/app".to_string()]).expect("Failed to set entrypoint");
    assert_eq!(image.config.config.entrypoint, Some(vec!["/app".to_string()]));

    image = oci_image::mutate::set_working_dir(image, "/app".to_string()).expect("Failed to set working dir");
    assert_eq!(image.config.config.working_dir, Some("/app".to_string()));

    image = oci_image::mutate::set_label(image, "version", "1.0").expect("Failed to set label");
    assert_eq!(
        image.config.config.labels.as_ref().unwrap().get("version"),
        Some(&"1.0".to_string())
    );
}

#[tokio::test]
async fn test_container_config_env() {
    let mut config = ContainerConfig::default();
    config.set_env("PATH", "/usr/local/bin:/usr/bin:/bin");
    config.set_env("HOME", "/root");
    config.set_env("PATH", "/custom/path");

    assert_eq!(config.get_env("PATH"), Some("/custom/path".to_string()));
    assert_eq!(config.get_env("HOME"), Some("/root".to_string()));
    assert_eq!(config.get_env("MISSING"), None);
    assert_eq!(config.env.as_ref().unwrap().len(), 2);
}

#[tokio::test]
async fn test_dockerignore_integration() {
    use kaniko_snapshot::walker::{parse_dockerignore, is_ignored};

    let content = r#"
*.log
!important.log
node_modules/
**/build/
"#;
    let patterns = parse_dockerignore(content);
    assert_eq!(patterns.len(), 4);
    assert!(is_ignored(std::path::Path::new("debug.log"), &patterns, false));
    assert!(!is_ignored(std::path::Path::new("important.log"), &patterns, false));
    assert!(is_ignored(std::path::Path::new("node_modules"), &patterns, true));
    assert!(is_ignored(std::path::Path::new("src/build"), &patterns, true));
}

#[tokio::test]
async fn test_image_serde_roundtrip() {
    let image = MutableImage::empty();
    let manifest_json = serde_json::to_string(&image.manifest).expect("Failed to serialize");
    let deserialized: oci_image::manifest::Manifest =
        serde_json::from_str(&manifest_json).expect("Failed to deserialize");
    assert_eq!(image.manifest, deserialized);

    let config_json = serde_json::to_string(&image.config).expect("Failed to serialize");
    let deserialized_config: oci_image::config::ImageConfig =
        serde_json::from_str(&config_json).expect("Failed to deserialize");
    assert_eq!(image.config, deserialized_config);
}

#[tokio::test]
async fn test_credential_anonymous() {
    let cred = kaniko_creds::Credential::anonymous();
    assert!(cred.is_anonymous());
    assert!(cred.username.is_empty());
    assert!(cred.password.is_empty());
}