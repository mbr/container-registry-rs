mod config;
mod podman;
pub(crate) mod registry;
mod reverse_proxy;

use std::{
    env, fs,
    net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs},
    path::Path,
    str::FromStr,
    sync::Arc,
};

use anyhow::Context;
use axum::{async_trait, Router};
use config::Config;
use gethostname::gethostname;
use podman::Podman;
use registry::{
    storage::ImageLocation, ContainerRegistry, ManifestReference, Reference, RegistryHooks,
};
use reverse_proxy::{PublishedContainer, ReverseProxy};
use sec::Secret;
use serde::{Deserialize, Deserializer};
use tower_http::trace::TraceLayer;
use tracing::{debug, error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

macro_rules! try_quiet {
    ($ex:expr, $msg:expr) => {
        match $ex {
            Ok(v) => v,
            Err(err) => {
                error!(%err, $msg);
                return;
            }
        }
    };
}

struct PodmanHook {
    podman: Podman,
    reverse_proxy: Arc<ReverseProxy>,
    local_addr: SocketAddr,
    registry_credentials: (String, Secret<String>),
}

impl PodmanHook {
    fn new<P: AsRef<Path>>(
        podman_path: P,
        reverse_proxy: Arc<ReverseProxy>,
        local_addr: SocketAddr,
        registry_credentials: (String, Secret<String>),
    ) -> Self {
        let podman = Podman::new(podman_path, podman_is_remote());
        Self {
            podman,
            reverse_proxy,
            local_addr,
            registry_credentials,
        }
    }

    async fn fetch_running_containers(&self) -> anyhow::Result<Vec<ContainerJson>> {
        debug!("refreshing running containers");

        let value = self.podman.ps(false).await?;
        let rv: Vec<ContainerJson> = serde_json::from_value(value)?;

        debug!(?rv, "fetched containers");

        Ok(rv)
    }

    async fn updated_published_set(&self) {
        let running: Vec<_> = try_quiet!(
            self.fetch_running_containers().await,
            "could not fetch running containers"
        )
        .iter()
        .filter_map(ContainerJson::published_container)
        .collect();

        info!(?running, "updating running container set");
        self.reverse_proxy
            .update_containers(running.into_iter())
            .await;
    }
}

pub(crate) fn podman_is_remote() -> bool {
    env::var("PODMAN_IS_REMOTE").unwrap_or_default() == "true"
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
#[allow(dead_code)]
struct ContainerJson {
    id: String,
    names: Vec<String>,
    #[serde(deserialize_with = "nullable_array")]
    ports: Vec<PortMapping>,
}

impl ContainerJson {
    fn image_location(&self) -> Option<ImageLocation> {
        const PREFIX: &str = "rockslide-";

        for name in &self.names {
            if let Some(subname) = name.strip_prefix(PREFIX) {
                if let Some((left, right)) = subname.split_once('-') {
                    return Some(ImageLocation::new(left.to_owned(), right.to_owned()));
                }
            }
        }

        None
    }

    fn active_published_port(&self) -> Option<&PortMapping> {
        self.ports.get(0)
    }

    fn published_container(&self) -> Option<PublishedContainer> {
        let image_location = self.image_location()?;
        let port_mapping = self.active_published_port()?;

        Some(PublishedContainer::new(
            port_mapping.get_host_listening_addr()?,
            image_location,
        ))
    }
}

fn nullable_array<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    let opt: Option<Vec<T>> = Deserialize::deserialize(deserializer)?;

    Ok(opt.unwrap_or_default())
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PortMapping {
    host_ip: String,
    container_port: u16,
    host_port: u16,
    range: u16,
    protocol: String,
}

impl PortMapping {
    fn get_host_listening_addr(&self) -> Option<SocketAddr> {
        let ip = Ipv4Addr::from_str(&self.host_ip).ok()?;

        Some((ip, self.host_port).into())
    }
}

#[async_trait]
impl RegistryHooks for PodmanHook {
    async fn on_manifest_uploaded(&self, manifest_reference: &ManifestReference) {
        // TODO: Make configurable?
        let production_tag = "prod";

        if matches!(manifest_reference.reference(), Reference::Tag(tag) if tag == production_tag) {
            let location = manifest_reference.location();
            let name = format!("rockslide-{}-{}", location.repository(), location.image());

            info!(%name, "removing (potentially nonexistant) container");
            try_quiet!(
                self.podman.rm(&name, true).await,
                "failed to remove container"
            );

            let image_url = format!(
                "{}/{}/{}:{}",
                self.local_addr,
                location.repository(),
                location.image(),
                production_tag
            );

            info!(%name, "loggging in");
            try_quiet!(
                self.podman
                    .login(
                        &self.registry_credentials.0,
                        self.registry_credentials.1.as_str(),
                        self.local_addr.to_string().as_ref(),
                        false
                    )
                    .await,
                "failed to login to local registry"
            );

            // We always pull the container to ensure we have the latest version.
            info!(%name, "pulling container");
            try_quiet!(
                self.podman.pull(&image_url).await,
                "failed to pull container"
            );

            info!(%name, "starting container");
            try_quiet!(
                self.podman
                    .run(&image_url)
                    .rm()
                    .rmi()
                    .name(name)
                    .tls_verify(false)
                    .publish("127.0.0.1::8000")
                    .env("PORT", "8000")
                    .execute()
                    .await,
                "failed to launch container"
            );

            info!(?manifest_reference, "new production image uploaded");

            self.updated_published_set().await;
        }
    }
}

fn load_config() -> anyhow::Result<Config> {
    match env::args().len() {
        0 | 1 => Ok(Default::default()),
        2 => {
            let arg = env::args().nth(1).expect("should have arg 1");
            let contents = fs::read_to_string(&arg)
                .context("could not read configuration file")
                .context(arg)?;
            let cfg = toml::from_str(&contents).context("failed to parse configuration")?;

            Ok(cfg)
        }
        _ => Err(anyhow::anyhow!(
            "expected at most one command arg, pointing to a config file"
        )),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Parse configuration, if available, otherwise use a default.
    let cfg = load_config().context("could not load configuration")?;

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| (&cfg.rockslide.log).into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    debug!(?cfg, "loaded configuration");

    let local_ip: IpAddr = if podman_is_remote() {
        info!("podman is remote, trying to guess IP address");
        let local_hostname = gethostname();
        let dummy_addr = (
            local_hostname
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("local hostname is not valid UTF8"))?,
            12345,
        )
            .to_socket_addrs()
            .ok()
            .and_then(|addrs| addrs.into_iter().next())
            .ok_or_else(|| anyhow::anyhow!("failed to resolve local hostname"))?;
        dummy_addr.ip()
    } else {
        [127, 0, 0, 1].into()
    };

    let local_addr = SocketAddr::from((local_ip, cfg.reverse_proxy.http_bind.port()));
    info!(%local_addr, "guessing local registry address");

    let reverse_proxy = ReverseProxy::new();

    let credentials = (
        "rockslide-podman".to_owned(),
        cfg.rockslide.master_key.as_secret_string(),
    );
    let hooks = PodmanHook::new(
        &cfg.containers.podman_path,
        reverse_proxy.clone(),
        local_addr,
        credentials,
    );
    hooks.updated_published_set().await;

    let registry =
        ContainerRegistry::new(&cfg.registry.storage_path, hooks, cfg.rockslide.master_key)?;

    let app = Router::new()
        .merge(registry.make_router())
        .merge(reverse_proxy.make_router())
        .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(cfg.reverse_proxy.http_bind)
        .await
        .context("failed to bind listener")?;
    axum::serve(listener, app)
        .await
        .context("http server exited with error")?;

    Ok(())
}
