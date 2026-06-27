use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use clap::Parser;
use docker_api::Docker;
use docker_api::opts::{
    ContainerFilter, ContainerListOpts, EventFilter, EventFilterType, EventsOpts,
};
use futures_util::stream::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::error::Error;
use std::sync::{Arc, RwLock};
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tokio::time::{Duration, sleep};
use tower_http::trace::TraceLayer;
use tracing_subscriber::prelude::*;

type SharedState = Arc<RwLock<AppState>>;

struct AppState {
    docker_path: String,
    managed_containers: ManagedContainers,
}

#[derive(Clone, Debug)]
struct DockerComposeInfo {
    config_files: String,
    project: String,
    service: String,
    working_dir: String,
}

struct ManagedContainers {
    containers: HashMap<String, DockerComposeInfo>,
}

fn compose_info_from_labels(
    labels: &HashMap<String, String>,
) -> Option<(String, DockerComposeInfo)> {
    if let (Some(token), Some(service), Some(project), Some(config_files), Some(working_dir)) = (
        labels.get("bell.token"),
        labels.get("com.docker.compose.service"),
        labels.get("com.docker.compose.project"),
        labels.get("com.docker.compose.project.config_files"),
        labels.get("com.docker.compose.project.working_dir"),
    ) {
        Some((
            token.clone(),
            DockerComposeInfo {
                project: project.clone(),
                service: service.clone(),
                config_files: config_files.clone(),
                working_dir: working_dir.clone(),
            },
        ))
    } else {
        None
    }
}

impl ManagedContainers {
    pub fn new() -> Self {
        ManagedContainers {
            containers: HashMap::new(),
        }
    }

    pub fn add(&mut self, labels: HashMap<String, String>) {
        if let Some((token, info)) = compose_info_from_labels(&labels) {
            self.containers.insert(token.clone(), info);
            tracing::info!(
                "adding {}: {}/{}",
                token,
                labels
                    .get("com.docker.compose.project")
                    .map(|s| s.as_str())
                    .unwrap_or("<unknown>"),
                labels
                    .get("com.docker.compose.service")
                    .map(|s| s.as_str())
                    .unwrap_or("<unknown>")
            );
        }
    }

    pub fn remove(&mut self, token: &str) {
        self.containers.remove(token);
        tracing::info!("removing {}", token);
    }

    pub fn get(&self, token: &str) -> Option<DockerComposeInfo> {
        self.containers.get(token).cloned()
    }
}

async fn initialize_containers(state: SharedState, docker_path: String) {
    let docker = Docker::unix(docker_path.as_str());
    let containers = docker.containers();
    let opts = ContainerListOpts::builder()
        .filter(vec![ContainerFilter::LabelKey("bell.token".to_string())])
        .build();
    let containers = containers.list(&opts).await.unwrap();
    let mut state = state.write().unwrap();
    for container in containers {
        if let Some(labels) = container.labels {
            state.managed_containers.add(labels);
        }
    }
}

async fn refresh_container_from_docker(
    state: SharedState,
    token: &str,
) -> Option<DockerComposeInfo> {
    let docker_path = { state.read().unwrap().docker_path.clone() };
    let docker = Docker::unix(docker_path.as_str());
    let opts = ContainerListOpts::builder()
        .all(true)
        .filter(vec![ContainerFilter::LabelKey("bell.token".to_string())])
        .build();
    let containers = docker.containers().list(&opts).await.ok()?;

    for container in containers {
        let Some(labels) = container.labels else {
            continue;
        };

        if labels.get("bell.token").is_some_and(|t| t == token)
            && let Some((_, info)) = compose_info_from_labels(&labels)
        {
            let info_clone = info.clone();
            state.write().unwrap().managed_containers.add(labels);
            return Some(info_clone);
        }
    }

    None
}

async fn handle_docker_events(state: SharedState, docker_path: String) {
    let docker = Docker::unix(docker_path.as_str());
    let opts = EventsOpts::builder()
        .filter(vec![
            EventFilter::Label("bell.token".to_string()),
            EventFilter::Type(EventFilterType::Container),
        ])
        .build();

    loop {
        let mut event_stream = docker.events(&opts);
        while let Some(event) = event_stream.next().await {
            let event = match event {
                Ok(event) => event,
                Err(err) => {
                    tracing::warn!("Docker event stream error: {}", err);
                    break;
                }
            };

            let (Some(action), Some(type_), Some(actor)) = (event.action, event.type_, event.actor)
            else {
                continue;
            };
            if type_ != "container" {
                continue;
            }
            let Some(attributes) = actor.attributes else {
                continue;
            };
            let Some(token) = attributes.get("bell.token").cloned() else {
                continue;
            };

            match action.as_str() {
                "die" => {
                    state.write().unwrap().managed_containers.remove(&token);
                }
                "start" => {
                    state.write().unwrap().managed_containers.add(attributes);
                }
                _ => {}
            }
        }

        tracing::warn!("Docker event stream ended, reconnecting");
        sleep(Duration::from_secs(1)).await;
    }
}

async fn run_docker_compose(
    info: &DockerComposeInfo,
    command: Vec<String>,
) -> Result<(), Box<dyn Error>> {
    let docker_path = "/usr/bin/docker";
    let mut args = vec![
        "compose".to_string(),
        "--project-directory".to_string(),
        info.working_dir.clone(),
        "--file".to_string(),
        info.config_files.clone(),
        "--project-name".to_string(),
        info.project.clone(),
    ];
    args.extend(command);
    tracing::debug!("running: docker compose {:?}", args);
    let output = tokio::process::Command::new(docker_path)
        .args(args)
        .output()
        .await?;
    if output.status.success() {
        Ok(())
    } else {
        Err(Box::new(std::io::Error::other(format!(
            "Failed to run docker compose\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ))))
    }
}

async fn run_git(info: &DockerComposeInfo, command: Vec<String>) -> Result<(), Box<dyn Error>> {
    let git_path = "/usr/bin/git";
    let mut args = vec!["-C".to_string(), info.working_dir.clone()];
    args.extend(command);
    tracing::debug!("running: git {:?}", args);
    let output = tokio::process::Command::new(git_path)
        .args(args)
        .output()
        .await?;
    if output.status.success() {
        Ok(())
    } else {
        Err(Box::new(std::io::Error::other(format!(
            "Failed to run git\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ))))
    }
}

async fn run_server(state: SharedState, address: String) {
    let app = Router::new()
        .route("/rebuild", post(post_rebuild))
        .with_state(state)
        .layer(TraceLayer::new_for_http());

    let listener = TcpListener::bind(address.as_str()).await.unwrap();
    tracing::debug!("listening on {}", address);
    axum::serve(listener, app).await.unwrap();
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ServerErrorRes {
    pub error: String,
}

type HandlerResult<T> = Result<T, (StatusCode, Json<ServerErrorRes>)>;

fn default_remote() -> String {
    "origin".to_string()
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PostRebuildReq {
    pub key: String,
    pub build_args: Option<Vec<String>>,
    pub branch: Option<String>,
    #[serde(default = "default_remote")]
    pub remote: String,
    #[serde(default)]
    pub all_services: bool,
}

async fn post_rebuild(
    State(state): State<SharedState>,
    Json(req): Json<PostRebuildReq>,
) -> HandlerResult<String> {
    tracing::debug!("received: {:?}", req);

    let info = {
        let guard = state.read().unwrap();
        guard.managed_containers.get(&req.key)
    };

    let info = if let Some(info) = info {
        info
    } else if let Some(info) = refresh_container_from_docker(state.clone(), &req.key).await {
        info
    } else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ServerErrorRes {
                error: "Container not found".to_string(),
            }),
        ));
    };

    let mut args = vec![
        "build".to_string(),
        "--no-cache".to_string(),
        "--quiet".to_string(),
    ];
    if let Some(build_args) = req.build_args {
        args.extend(build_args);
    }
    if !req.all_services {
        args.extend(vec![info.service.clone()]);
    }
    async {
        if let Some(branch) = req.branch {
            run_git(
                &info,
                vec![
                    "pull".to_string(),
                    "--ff-only".to_string(),
                    req.remote,
                    branch,
                ],
            )
            .await?;
        }
        run_docker_compose(&info, args).await?;
        run_docker_compose(&info, vec!["up".to_string(), "--detach".to_string()]).await?;
        Ok("OK".to_string())
    }
    .await
    .map_err(|e: Box<dyn Error>| {
        tracing::error!("Error: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ServerErrorRes {
                error: e.to_string(),
            }),
        )
    })
}

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[arg(long, default_value = "/var/run/docker.sock")]
    docker_path: String,

    #[arg(long, default_value = "0.0.0.0:8080")]
    address: String,
}

#[tokio::main]
async fn main() {
    let filter = tracing_subscriber::filter::Targets::new()
        .with_target("tower_http::trace::on_response", tracing::Level::DEBUG)
        .with_target("tower_http::trace::on_request", tracing::Level::DEBUG)
        .with_target("tower_http::trace::make_span", tracing::Level::DEBUG);
    let tracing_layer = tracing_subscriber::fmt::layer();
    tracing_subscriber::registry()
        .with(tracing_layer)
        .with(filter)
        .init();

    let args = Cli::parse();

    let shared_state = Arc::new(RwLock::new(AppState {
        docker_path: args.docker_path.clone(),
        managed_containers: ManagedContainers::new(),
    }));

    initialize_containers(shared_state.clone(), args.docker_path.clone()).await;

    let mut tasks = JoinSet::new();

    tasks.spawn(handle_docker_events(
        shared_state.clone(),
        args.docker_path.clone(),
    ));

    tasks.spawn(run_server(shared_state.clone(), args.address));

    while tasks.join_next().await.is_some() {
        tracing::debug!("task finished");
    }
}
