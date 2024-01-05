use std::collections::HashMap;
use std::fs;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::str::FromStr;
use std::{net::SocketAddr, path::Path, sync::Arc};

use crate::podman::podman_is_remote;
use crate::{
    podman::Podman,
    registry::{storage::ImageLocation, ManifestReference, Reference, RegistryHooks},
    reverse_proxy::ReverseProxy,
};

use anyhow::Context;
use axum::async_trait;
use sec::Secret;
use serde::{Deserialize, Deserializer, Serialize};
use tracing::{debug, error, info};

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

pub(crate) struct ContainerOrchestrator {
    podman: Podman,
    reverse_proxy: Arc<ReverseProxy>,
    local_addr: SocketAddr,
    registry_credentials: (String, Secret<String>),
    configs_dir: PathBuf,
}

#[derive(Clone, Debug)]
pub(crate) struct PublishedContainer {
    host_addr: SocketAddr,
    manifest_reference: ManifestReference,
    config: RuntimeConfig,
}

impl PublishedContainer {
    pub(crate) fn manifest_reference(&self) -> &ManifestReference {
        &self.manifest_reference
    }

    pub(crate) fn host_addr(&self) -> SocketAddr {
        self.host_addr
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct RuntimeConfig {
    #[serde(default)]
    http_access: Option<HashMap<String, String>>,
}

impl ContainerOrchestrator {
    pub(crate) fn new<P: AsRef<Path>, Q: AsRef<Path>>(
        podman_path: P,
        reverse_proxy: Arc<ReverseProxy>,
        local_addr: SocketAddr,
        registry_credentials: (String, Secret<String>),
        runtime_dir: Q,
    ) -> anyhow::Result<Self> {
        let podman = Podman::new(podman_path, podman_is_remote());

        let configs_dir = runtime_dir
            .as_ref()
            .canonicalize()
            .context("could not canonicalize runtime config dir")?
            .join("configs");

        if !configs_dir.exists() {
            fs::create_dir(&configs_dir).context("could not create config dir")?;
        }

        Ok(Self {
            podman,
            reverse_proxy,
            local_addr,
            registry_credentials,
            configs_dir,
        })
    }

    fn config_path(&self, manifest_reference: &ManifestReference) -> PathBuf {
        let location = manifest_reference.location();

        self.configs_dir
            .join(location.repository())
            .join(location.image())
            .join(manifest_reference.reference().to_string())
    }

    pub(crate) async fn load_config(
        &self,
        manifest_reference: &ManifestReference,
    ) -> anyhow::Result<RuntimeConfig> {
        let config_path = self.config_path(manifest_reference);

        if !config_path.exists() {
            return Ok(Default::default());
        }

        let raw = tokio::fs::read_to_string(config_path)
            .await
            .context("could not read config")?;

        toml::from_str(&raw).context("could not parse configuration")
    }

    async fn fetch_managed_containers(&self, all: bool) -> anyhow::Result<Vec<PublishedContainer>> {
        debug!("refreshing running containers");

        let value = self.podman.ps(all).await?;
        let all_containers: Vec<ContainerJson> = serde_json::from_value(value)?;

        debug!(?all_containers, "fetched containers");

        let mut rv = Vec::new();
        for container in all_containers {
            // TODO: Just log error instead of returning.
            if let Some(pc) = self.load_managed_container(container).await? {
                rv.push(pc);
            }
        }
        Ok(rv)
    }

    async fn load_managed_container(
        &self,
        container_json: ContainerJson,
    ) -> anyhow::Result<Option<PublishedContainer>> {
        let manifest_reference = if let Some(val) = container_json.manifest_reference() {
            val
        } else {
            return Ok(None);
        };

        let port_mapping = if let Some(val) = container_json.active_published_port() {
            val
        } else {
            return Ok(None);
        };

        let config = self.load_config(&manifest_reference).await?;

        Ok(Some(PublishedContainer {
            host_addr: port_mapping
                .get_host_listening_addr()
                .context("could not get host listening address")?,
            manifest_reference,
            config,
        }))
    }

    pub(crate) async fn updated_published_set(&self) {
        let running: Vec<_> = try_quiet!(
            self.fetch_managed_containers(false).await,
            "could not fetch running containers"
        );

        info!(?running, "updating running container set");
        self.reverse_proxy
            .update_containers(running.into_iter())
            .await;
    }

    async fn synchronize_container_state(&self, manifest_reference: &ManifestReference) {
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

            info!(?manifest_reference, "new production image running");
        }
    }

    pub(crate) async fn synchronize_all(&self) -> anyhow::Result<()> {
        for container in self.fetch_managed_containers(true).await? {
            self.synchronize_container_state(container.manifest_reference())
                .await;
        }

        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
#[allow(dead_code)]
struct ContainerJson {
    id: String,
    image: String,
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

    fn image_tag(&self) -> Option<Reference> {
        let idx = self.image.rfind(':')?;

        // TODO: Handle Reference::Digest here.
        Some(Reference::Tag(self.image[idx..].to_owned()))
    }

    fn manifest_reference(&self) -> Option<ManifestReference> {
        Some(ManifestReference::new(
            self.image_location()?,
            self.image_tag()?,
        ))
    }

    fn active_published_port(&self) -> Option<&PortMapping> {
        self.ports.get(0)
    }
}

#[async_trait]
impl RegistryHooks for ContainerOrchestrator {
    async fn on_manifest_uploaded(&self, manifest_reference: &ManifestReference) {
        self.synchronize_container_state(manifest_reference).await;

        self.updated_published_set().await;
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
