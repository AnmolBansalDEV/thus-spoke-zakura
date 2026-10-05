use std::{
    fmt::{self, Display},
    fs,
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    str::FromStr,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

const APP_IMAGE_REPOSITORY: &str = "ghcr.io/zcashlabs/thus-spoke-zakura-app";
const ZAKURA_IMAGE: &str = "zakuracore/zakura:1.6.0";
const LIGHTWALLETD_IMAGE_REPOSITORY: &str = "ghcr.io/zcashlabs/thus-spoke-zakura-lightwalletd";

fn app_image() -> String {
    format!("{APP_IMAGE_REPOSITORY}:{}", env!("CARGO_PKG_VERSION"))
}

fn lightwalletd_image() -> String {
    format!(
        "{LIGHTWALLETD_IMAGE_REPOSITORY}:{}",
        env!("CARGO_PKG_VERSION")
    )
}

#[derive(Clone, Debug)]
pub struct InstanceName(String);

impl Display for InstanceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for InstanceName {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let valid = !value.is_empty()
            && value.len() <= 40
            && value
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !valid || value.starts_with('-') || value.ends_with('-') {
            bail!("instance names use 1-40 lowercase letters, digits, or internal hyphens");
        }
        Ok(Self(value.to_owned()))
    }
}

fn default_regtest() -> String {
    "regtest".to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoints {
    pub dashboard: String,
    pub rpc: String,
    pub lightwalletd: String,
    pub p2p: String,
    #[serde(default = "default_regtest")]
    pub network: String,
    #[serde(default)]
    pub tls: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Instance {
    name: String,
    version: u32,
    endpoints: Endpoints,
}

#[derive(Debug, Deserialize, Serialize)]
struct MineResult {
    blocks: usize,
    hashes: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct FaucetResult {
    address: String,
    amount_zatoshi: u64,
    txid: String,
    block_hash: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct Activity {
    id: String,
    kind: String,
    from_account: Option<u8>,
    to_account: u8,
    source_pool: String,
    destination_pool: String,
    amount_zatoshi: u64,
    txid: String,
    block_hash: Option<String>,
    status: String,
}

pub struct Runtime {
    root: PathBuf,
}

impl Runtime {
    pub fn discover() -> Result<Self> {
        let dirs = ProjectDirs::from("com", "zakura", "thus-spoke-zakura")
            .ok_or_else(|| anyhow!("could not determine the platform configuration directory"))?;
        Ok(Self {
            root: dirs.config_dir().to_owned(),
        })
    }

    pub fn doctor(&self, json: bool) -> Result<()> {
        let docker = docker_output(["version", "--format", "{{.Server.Version}}"]);
        let result = serde_json::json!({
            "docker": docker.as_ref().ok(),
            "config_dir": self.root,
            "ok": docker.is_ok(),
        });
        if json {
            println!("{}", serde_json::to_string_pretty(&result)?);
        } else if let Ok(version) = docker {
            println!("✓ Docker {version}\n✓ Config: {}", self.root.display());
        } else {
            bail!("Docker is not reachable; start Docker Desktop or the Docker daemon");
        }
        Ok(())
    }

    pub fn build(&self, dev: bool) -> Result<()> {
        self.doctor(false)?;
        build_project_images(dev)?;
        ensure_image(ZAKURA_IMAGE)?;
        println!("Runtime images are ready.");
        Ok(())
    }

    pub fn pull(&self) -> Result<()> {
        self.doctor(false)?;
        for image in [app_image(), lightwalletd_image(), ZAKURA_IMAGE.to_owned()] {
            println!("Pulling {image}…");
            docker(["pull", &image])?;
        }
        println!("Runtime images are ready.");
        Ok(())
    }

    pub fn start(
        &self,
        name: &InstanceName,
        no_open: bool,
        json: bool,
        port_offset: u16,
    ) -> Result<()> {
        self.doctor(false)?;
        for image in [app_image(), lightwalletd_image(), ZAKURA_IMAGE.to_owned()] {
            require_image(&image)?;
        }
        let shutdown = Shutdown::install()?;
        self.start_with(name, no_open, json, port_offset, &DockerHost, &shutdown)
    }

    pub fn status(&self, name: &InstanceName, json: bool) -> Result<()> {
        let endpoints = self
            .read_instance(name)
            .map(|i| i.endpoints)
            .or_else(|_| inspect_endpoints(&prefix(name)))?;
        let running = container_running(&format!("{}-app", prefix(name)))?;
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"name": name.to_string(), "running": running, "endpoints": endpoints})
                )?
            );
        } else {
            println!("{}", status_text(name, running, &endpoints));
        }
        Ok(())
    }

    pub fn endpoints(&self, name: &InstanceName, json: bool) -> Result<()> {
        let endpoints = self.read_instance(name)?.endpoints;
        if json {
            println!("{}", serde_json::to_string_pretty(&endpoints)?);
        } else {
            println!("{}", endpoint_lines(&endpoints));
        }
        Ok(())
    }

    pub fn open(&self, name: &InstanceName) -> Result<()> {
        open_url(&self.read_instance(name)?.endpoints.dashboard)
    }

    pub fn mine(&self, name: &InstanceName, blocks: u32, json: bool) -> Result<()> {
        let app_container = format!("{}-app", prefix(name));
        if !container_running(&app_container)? {
            bail!("environment {name} is not running; start it with `ths --name {name}`");
        }
        let dashboard = self.read_instance(name)?.endpoints.dashboard;
        let response = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()?
            .post(format!("{dashboard}/api/v1/mine"))
            .json(&serde_json::json!({"blocks": blocks}))
            .send()
            .with_context(|| format!("asking environment {name} to mine {blocks} blocks"))?;
        let status = response.status();
        if !status.is_success() {
            let detail = response
                .text()
                .unwrap_or_else(|_| "response body was unreadable".to_owned());
            bail!("environment {name} rejected mining ({status}): {detail}");
        }
        let result: MineResult = response.json().context("decoding mining response")?;
        if json {
            println!("{}", serde_json::to_string_pretty(&result)?);
        } else {
            println!("Mined {} blocks on {name}.", result.blocks);
            if let Some(tip) = result.hashes.last() {
                println!("New tip: {tip}");
            }
        }
        Ok(())
    }

    pub fn faucet(
        &self,
        name: &InstanceName,
        address: &str,
        amount_zatoshi: u64,
        json: bool,
    ) -> Result<()> {
        let app_container = format!("{}-app", prefix(name));
        if !container_running(&app_container)? {
            bail!("environment {name} is not running; start it with `ths --name {name}`");
        }
        let dashboard = self.read_instance(name)?.endpoints.dashboard;
        let response = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()?
            .post(format!("{dashboard}/api/v1/faucet/address"))
            .json(&serde_json::json!({
                "address": address,
                "amount_zatoshi": amount_zatoshi,
            }))
            .send()
            .with_context(|| format!("asking environment {name} to fund {address}"))?;
        let status = response.status();
        if !status.is_success() {
            let detail = response
                .text()
                .unwrap_or_else(|_| "response body was unreadable".to_owned());
            bail!("environment {name} rejected faucet request ({status}): {detail}");
        }
        let result: FaucetResult = response.json().context("decoding faucet response")?;
        if json {
            println!("{}", serde_json::to_string_pretty(&result)?);
        } else {
            println!(
                "Sent {} ZEC to {} on {name}.",
                format_zec(result.amount_zatoshi),
                result.address
            );
            println!("Transaction: {}", result.txid);
            println!("Confirmed in: {}", result.block_hash);
        }
        Ok(())
    }

    pub fn wallet_faucet(
        &self,
        name: &InstanceName,
        accounts: &[u8],
        amount_zatoshi: u64,
        pool: &str,
        json: bool,
    ) -> Result<()> {
        let app_container = format!("{}-app", prefix(name));
        if !container_running(&app_container)? {
            bail!("environment {name} is not running; start it with `ths --name {name}`");
        }
        let dashboard = self.read_instance(name)?.endpoints.dashboard;
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()?;
        let mut funded = Vec::new();
        let mut failures = Vec::new();
        for &account_id in accounts {
            let idempotency_key =
                format!("ths-wallet-faucet-{account_id}-{}", uuid::Uuid::new_v4());
            let outcome = client
                .post(format!("{dashboard}/api/v1/faucet"))
                .json(&serde_json::json!({
                    "account_id": account_id,
                    "pool": pool,
                    "amount_zatoshi": amount_zatoshi,
                    "idempotency_key": idempotency_key,
                }))
                .send()
                .with_context(|| format!("asking environment {name} to fund account {account_id}"));
            match outcome.and_then(decode_activity) {
                Ok(activity) => funded.push(activity),
                Err(error) => failures.push(format!("account {account_id}: {error:#}")),
            }
        }
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"funded": funded, "failed": failures})
                )?
            );
        } else {
            for activity in &funded {
                println!(
                    "Funded account {} with {} ZEC ({} pool) on {name}.",
                    activity.to_account,
                    format_zec(activity.amount_zatoshi),
                    activity.destination_pool
                );
                println!("  Transaction: {}", activity.txid);
            }
            for failure in &failures {
                eprintln!("Failed to fund {failure}");
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            bail!(
                "{} of {} faucet requests failed",
                failures.len(),
                accounts.len()
            )
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn wallet_send(
        &self,
        name: &InstanceName,
        from: u8,
        to: u8,
        source_pool: &str,
        destination_pool: &str,
        amount_zatoshi: u64,
        memo: Option<&str>,
        json: bool,
    ) -> Result<()> {
        let app_container = format!("{}-app", prefix(name));
        if !container_running(&app_container)? {
            bail!("environment {name} is not running; start it with `ths --name {name}`");
        }
        let dashboard = self.read_instance(name)?.endpoints.dashboard;
        let idempotency_key = format!("ths-wallet-send-{}", uuid::Uuid::new_v4());
        let response = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()?
            .post(format!("{dashboard}/api/v1/send"))
            .json(&serde_json::json!({
                "from_account": from,
                "to_account": to,
                "source_pool": source_pool,
                "destination_pool": destination_pool,
                "amount_zatoshi": amount_zatoshi,
                "idempotency_key": idempotency_key,
                "memo": memo,
            }))
            .send()
            .with_context(|| {
                format!("asking environment {name} to send from account {from} to account {to}")
            })?;
        let activity = decode_activity(response)?;
        if json {
            println!("{}", serde_json::to_string_pretty(&activity)?);
        } else {
            println!(
                "Sent {} ZEC from account {} ({} pool) to account {} ({} pool) on {name}.",
                format_zec(activity.amount_zatoshi),
                from,
                activity.source_pool,
                activity.to_account,
                activity.destination_pool
            );
            if let Some(memo) = memo {
                println!("Memo: {memo}");
            }
            println!("Transaction: {}", activity.txid);
            if let Some(block_hash) = &activity.block_hash {
                println!("Confirmed in: {block_hash}");
            }
        }
        Ok(())
    }

    pub fn logs(&self, name: &InstanceName, service: Option<&str>, follow: bool) -> Result<()> {
        let service = service.unwrap_or("app");
        let mut args = vec!["logs"];
        if follow {
            args.push("--follow");
        }
        let container = format!("{}-{service}", prefix(name));
        args.push(&container);
        docker_inherit(&args)
    }

    pub fn stop(&self, name: &InstanceName) -> Result<()> {
        self.delete_instance_resources(name)?;
        println!("Stopped and deleted {name} and all of its development data.");
        Ok(())
    }

    pub fn reset(&self, name: &InstanceName, force: bool) -> Result<()> {
        if !force {
            bail!("reset deletes chain, wallet, and seed data; repeat with --force");
        }
        self.delete_instance_resources(name)?;
        println!("Deleted {name}; its Docker volumes cannot be recovered.");
        Ok(())
    }

    pub fn list(&self, json: bool) -> Result<()> {
        let mut instances = Vec::new();
        if self.root.exists() {
            for entry in fs::read_dir(&self.root)? {
                let path = entry?.path().join("instance.json");
                if path.exists() {
                    instances.push(serde_json::from_slice::<Instance>(&fs::read(path)?)?);
                }
            }
        }
        if json {
            println!("{}", serde_json::to_string_pretty(&instances)?);
        } else if instances.is_empty() {
            println!("No environments yet.");
        } else {
            for i in instances {
                println!("{:<20} {}", i.name, i.endpoints.dashboard);
            }
        }
        Ok(())
    }

    fn instance_dir(&self, name: &InstanceName) -> PathBuf {
        self.root.join(name.to_string())
    }
    fn write_instance(&self, name: &InstanceName, endpoints: &Endpoints) -> Result<()> {
        let instance = Instance {
            name: name.to_string(),
            version: 1,
            endpoints: endpoints.clone(),
        };
        fs::write(
            self.instance_dir(name).join("instance.json"),
            serde_json::to_vec_pretty(&instance)?,
        )?;
        Ok(())
    }
    fn read_instance(&self, name: &InstanceName) -> Result<Instance> {
        let path = self.instance_dir(name).join("instance.json");
        serde_json::from_slice(
            &fs::read(&path).with_context(|| format!("instance {name} does not exist"))?,
        )
        .context("invalid instance metadata")
    }

    fn delete_instance_resources(&self, name: &InstanceName) -> Result<()> {
        let prefix = prefix(name);
        let mut failures = Vec::new();
        for service in ["app", "lightwalletd", "zakura", "init"] {
            let target = format!("{prefix}-{service}");
            match container_exists(&target) {
                Ok(true) => {
                    if let Err(error) = docker(["rm", "-f", &target]) {
                        failures.push(format!("container {target}: {error}"));
                    }
                }
                Ok(false) => {}
                Err(error) => failures.push(format!("container {target}: {error}")),
            }
        }
        for suffix in ["chain", "wallet", "lightwalletd", "config"] {
            let volume = format!("{prefix}-{suffix}");
            match volume_exists(&volume) {
                Ok(true) => {
                    if let Err(error) = docker(["volume", "rm", &volume]) {
                        failures.push(format!("volume {volume}: {error}"));
                    }
                }
                Ok(false) => {}
                Err(error) => failures.push(format!("volume {volume}: {error}")),
            }
        }
        match network_exists(&prefix) {
            Ok(true) => {
                if let Err(error) = docker(["network", "rm", &prefix]) {
                    failures.push(format!("network {prefix}: {error}"));
                }
            }
            Ok(false) => {}
            Err(error) => failures.push(format!("network {prefix}: {error}")),
        }
        // Keep the metadata while Docker resources may remain, so the environment stays
        // listed and `ths stop` can be repeated once Docker recovers.
        let dir = self.instance_dir(name);
        if failures.is_empty()
            && dir.exists()
            && let Err(error) = fs::remove_dir_all(&dir)
        {
            failures.push(format!("metadata {}: {error}", dir.display()));
        }
        if failures.is_empty() {
            Ok(())
        } else {
            bail!(
                "could not delete every instance resource: {}",
                failures.join("; ")
            )
        }
    }
}

fn decode_activity(response: reqwest::blocking::Response) -> Result<Activity> {
    let status = response.status();
    if !status.is_success() {
        let detail = response
            .text()
            .unwrap_or_else(|_| "response body was unreadable".to_owned());
        bail!("rejected ({status}): {detail}");
    }
    response.json().context("decoding response")
}

fn format_zec(zatoshi: u64) -> String {
    let whole = zatoshi / 100_000_000;
    let fraction = zatoshi % 100_000_000;
    if fraction == 0 {
        whole.to_string()
    } else {
        format!("{whole}.{fraction:08}")
            .trim_end_matches('0')
            .to_owned()
    }
}

#[derive(Debug)]
struct HostPorts {
    dashboard: u16,
    rpc: u16,
    p2p: u16,
    lightwalletd: u16,
}

fn host_ports(offset: u16) -> Result<HostPorts> {
    if !offset.is_multiple_of(10) {
        bail!("--port-offset must be a multiple of 10 (got {offset})");
    }
    Ok(HostPorts {
        dashboard: 32805 + offset,
        rpc: 18232 + offset,
        p2p: 18233 + offset,
        lightwalletd: 9067 + offset,
    })
}

fn loopback_publish(host: u16, container: u16) -> String {
    format!("127.0.0.1:{host}:{container}")
}

fn require_free_loopback(port: u16) -> Result<()> {
    match std::net::TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => {
            drop(listener);
            Ok(())
        }
        Err(_) => bail!("port {port} is already in use on 127.0.0.1"),
    }
}

fn prefix(name: &InstanceName) -> String {
    format!("ths-{name}")
}
fn label(name: &InstanceName) -> String {
    format!("com.zakura.ths.instance={name}")
}

fn ensure_network(prefix: &str) -> Result<()> {
    if docker_output(["network", "inspect", prefix]).is_err() {
        docker(["network", "create", prefix])?;
    }
    Ok(())
}
fn ensure_volume(volume: &str, name: &InstanceName) -> Result<()> {
    if docker_output(["volume", "inspect", volume]).is_err() {
        docker(["volume", "create", "--label", &label(name), volume])?;
    }
    Ok(())
}
fn ensure_zakura(prefix: &str, name: &InstanceName, ports: &HostPorts) -> Result<()> {
    let target = format!("{prefix}-zakura");
    if !container_exists(&target)? {
        let rpc_bind = loopback_publish(ports.rpc, 18232);
        let p2p_bind = loopback_publish(ports.p2p, 18233);
        docker([
            "create",
            "--name",
            &target,
            "--network",
            prefix,
            "--network-alias",
            "zakura",
            "--label",
            &label(name),
            "-p",
            &rpc_bind,
            "-p",
            &p2p_bind,
            "-v",
            &format!("{prefix}-chain:/data"),
            "-v",
            &format!("{prefix}-config:/config:ro"),
            "-e",
            "CONFIG_FILE_PATH=/config/zakurad.toml",
            ZAKURA_IMAGE,
            "zakurad",
            "start",
        ])?;
    }
    Ok(())
}
fn ensure_lightwalletd(prefix: &str, name: &InstanceName, ports: &HostPorts) -> Result<()> {
    let target = format!("{prefix}-lightwalletd");
    if !container_exists(&target)? {
        let image = lightwalletd_image();
        let lightwalletd_bind = loopback_publish(ports.lightwalletd, 9067);
        docker([
            "create",
            "--name",
            &target,
            "--network",
            prefix,
            "--network-alias",
            "lightwalletd",
            "--label",
            &label(name),
            "--user",
            "0:0",
            "-p",
            &lightwalletd_bind,
            "-v",
            &format!("{prefix}-lightwalletd:/var/lib/lightwalletd"),
            &image,
            "--no-tls-very-insecure",
            "--grpc-bind-addr",
            "0.0.0.0:9067",
            "--rpchost",
            "zakura",
            "--rpcport",
            "18232",
            "--rpcuser",
            "unused",
            "--rpcpassword",
            "unused",
            "--data-dir",
            "/var/lib/lightwalletd",
            "--log-file",
            "/dev/stdout",
        ])?;
    }
    Ok(())
}
fn ensure_app(prefix: &str, name: &InstanceName, ports: &HostPorts) -> Result<()> {
    let target = format!("{prefix}-app");
    if !container_exists(&target)? {
        let public_rpc = format!("http://127.0.0.1:{}", ports.rpc);
        let public_lightwalletd = format!("http://127.0.0.1:{}", ports.lightwalletd);
        let public_p2p = format!("127.0.0.1:{}", ports.p2p);
        let dashboard_bind = loopback_publish(ports.dashboard, 8080);
        let image = app_image();
        docker([
            "create",
            "--name",
            &target,
            "--network",
            prefix,
            "--label",
            &label(name),
            "-p",
            &dashboard_bind,
            "-e",
            "THS_LISTEN=0.0.0.0:8080",
            "-e",
            "THS_ZAKURA_RPC=http://zakura:18232",
            "-e",
            "THS_LIGHTWALLETD=http://lightwalletd:9067",
            "-e",
            &format!("THS_INSTANCE={name}"),
            "-e",
            &format!("THS_PUBLIC_ZAKURA_RPC={public_rpc}"),
            "-e",
            &format!("THS_PUBLIC_LIGHTWALLETD={public_lightwalletd}"),
            "-e",
            &format!("THS_PUBLIC_P2P={public_p2p}"),
            "-v",
            &format!("{prefix}-wallet:/data"),
            &image,
            "serve",
            "--data-dir",
            "/data",
        ])?;
    }
    Ok(())
}

fn endpoints_for(ports: &HostPorts) -> Endpoints {
    Endpoints {
        dashboard: format!("http://127.0.0.1:{}", ports.dashboard),
        rpc: format!("http://127.0.0.1:{}", ports.rpc),
        lightwalletd: format!("http://127.0.0.1:{}", ports.lightwalletd),
        p2p: format!("127.0.0.1:{}", ports.p2p),
        network: default_regtest(),
        tls: false,
    }
}

fn inspect_endpoints(prefix: &str) -> Result<Endpoints> {
    Ok(Endpoints {
        dashboard: format!(
            "http://127.0.0.1:{}",
            published_port(&format!("{prefix}-app"), "8080/tcp")?
        ),
        rpc: format!(
            "http://127.0.0.1:{}",
            published_port(&format!("{prefix}-zakura"), "18232/tcp")?
        ),
        lightwalletd: format!(
            "http://127.0.0.1:{}",
            published_port(&format!("{prefix}-lightwalletd"), "9067/tcp")?
        ),
        p2p: format!(
            "127.0.0.1:{}",
            published_port(&format!("{prefix}-zakura"), "18233/tcp")?
        ),
        network: default_regtest(),
        tls: false,
    })
}
fn published_port(container: &str, port: &str) -> Result<u16> {
    docker_output([
        "inspect",
        "--format",
        &format!("{{{{(index (index .NetworkSettings.Ports \"{port}\") 0).HostPort}}}}"),
        container,
    ])?
    .parse()
    .context("Docker returned an invalid published port")
}
fn container_exists(name: &str) -> Result<bool> {
    docker_listed([
        "container",
        "ls",
        "--all",
        "--quiet",
        "--filter",
        &format!("name=^{name}$"),
    ])
}
fn volume_exists(name: &str) -> Result<bool> {
    docker_listed([
        "volume",
        "ls",
        "--quiet",
        "--filter",
        &format!("name=^{name}$"),
    ])
}
fn network_exists(name: &str) -> Result<bool> {
    docker_listed([
        "network",
        "ls",
        "--quiet",
        "--filter",
        &format!("name=^{name}$"),
    ])
}
/// `docker inspect` fails the same way for a missing object and an unreachable daemon, so
/// existence comes from a filtered listing: empty means absent, failure stays an error.
fn docker_listed<const N: usize>(args: [&str; N]) -> Result<bool> {
    Ok(!docker_output(args)?.is_empty())
}
fn ensure_image(image: &str) -> Result<()> {
    if docker_output(["image", "inspect", image]).is_err() {
        println!("Pulling {image}…");
        docker(["pull", image])?;
    }
    Ok(())
}
fn require_image(image: &str) -> Result<()> {
    if docker_output(["image", "inspect", image]).is_err() {
        bail!(
            "required image {image} is unavailable; run `ths pull` (or `ths build` from a source checkout) first"
        );
    }
    Ok(())
}
fn build_project_images(dev: bool) -> Result<()> {
    let project_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    if !project_root.join("Dockerfile").is_file()
        || !project_root
            .join("docker/lightwalletd.Dockerfile")
            .is_file()
    {
        bail!(
            "cannot build images: project source is unavailable at {}",
            project_root.display()
        );
    }

    let app_image = app_image();
    let lightwalletd_image = lightwalletd_image();
    if dev {
        println!("Building {app_image} with the Rust development profile…");
        docker_inherit_in(
            &[
                "build",
                "--build-arg",
                "RUST_PROFILE=dev-runtime",
                "-t",
                &app_image,
                ".",
            ],
            &project_root,
        )?;
    } else {
        println!("Building {app_image}…");
        docker_inherit_in(&["build", "-t", &app_image, "."], &project_root)?;
    }
    println!("Building {lightwalletd_image}…");
    docker_inherit_in(
        &[
            "build",
            "-f",
            "docker/lightwalletd.Dockerfile",
            "-t",
            &lightwalletd_image,
            ".",
        ],
        &project_root,
    )
}
struct Shutdown {
    flag: Arc<AtomicBool>,
    /// Lets a `DeletionMonitor` end `wait` through the same channel as Ctrl+C.
    sender: mpsc::Sender<Ended>,
    receiver: mpsc::Receiver<Ended>,
}

impl Shutdown {
    #[cfg(test)]
    fn channel() -> (mpsc::Sender<Ended>, Self) {
        let (sender, receiver) = mpsc::channel();
        let shutdown = Self {
            flag: Arc::new(AtomicBool::new(false)),
            sender: sender.clone(),
            receiver,
        };
        (sender, shutdown)
    }

    fn install() -> Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let flag = Arc::new(AtomicBool::new(false));
        let handler_flag = flag.clone();
        let handler_sender = sender.clone();
        ctrlc::set_handler(move || {
            handler_flag.store(true, Ordering::SeqCst);
            let _ = handler_sender.send(Ended::Interrupted);
        })
        .context("installing the shutdown signal handler")?;
        Ok(Self {
            flag,
            sender,
            receiver,
        })
    }

    fn try_interrupted(&self) -> bool {
        // Deletion notices only arrive while `wait` runs, which receives them itself.
        if matches!(self.receiver.try_recv(), Ok(Ended::Interrupted)) {
            self.flag.store(true, Ordering::SeqCst);
        }
        self.flag.load(Ordering::SeqCst)
    }

    fn check(&self) -> Result<()> {
        if self.try_interrupted() {
            bail!("interrupted");
        }
        Ok(())
    }

    /// Waits for a shutdown signal, or for a `DeletionMonitor` to report that another command
    /// (`ths stop` or `ths reset` from another shell) deleted the environment.
    fn wait(&self) -> Result<Ended> {
        // Not `try_interrupted`: it would drop a deletion notice that is already queued.
        if self.flag.load(Ordering::SeqCst) {
            return Ok(Ended::Interrupted);
        }
        let ended = self
            .receiver
            .recv()
            .context("waiting for a shutdown signal")?;
        if ended == Ended::Interrupted {
            self.flag.store(true, Ordering::SeqCst);
        }
        Ok(ended)
    }

    fn wait_timeout(&self, timeout: Duration) -> Result<()> {
        if self.try_interrupted() {
            bail!("interrupted");
        }
        match self.receiver.recv_timeout(timeout) {
            Ok(_) => {
                self.flag.store(true, Ordering::SeqCst);
                bail!("interrupted");
            }
            Err(RecvTimeoutError::Timeout) => Ok(()),
            Err(RecvTimeoutError::Disconnected) => {
                if self.try_interrupted() {
                    bail!("interrupted");
                }
                Err(anyhow!("waiting for a shutdown signal"))
            }
        }
    }
}

/// How a running environment's foreground `start` ended.
#[derive(Debug, PartialEq, Eq)]
enum Ended {
    /// Ctrl+C or a termination signal: `start` deletes the environment itself.
    Interrupted,
    /// Another command already deleted it.
    DeletedElsewhere,
}

/// How long deletion monitoring waits before retrying after a Docker error or a lost event stream.
const DELETION_WATCH_RETRY: Duration = Duration::from_secs(1);

/// The container whose deletion a running `start` watches. Its ID keeps a same-name replacement
/// from hiding the deletion, and its creation time (Docker's clock) lets a subscription replay
/// a destroy event that happened before it connected.
#[derive(Clone, Debug, PartialEq, Eq)]
struct WatchedContainer {
    id: String,
    created: String,
}

/// The Docker operations behind deletion monitoring, and its test seam.
trait ContainerEvents: Send + Sync {
    /// Identifies the container named `name`, or returns `None` once it no longer exists.
    fn identify(&self, name: &str) -> Result<Option<WatchedContainer>>;
    /// Whether the container with this ID still exists.
    fn exists(&self, id: &str) -> Result<bool>;
    /// Blocks until Docker reports that the container was destroyed (`Ok(true)`), or until the
    /// event stream ends without reporting it (`Ok(false)`).
    fn wait_destroyed(&self, container: &WatchedContainer) -> Result<bool>;
    /// Ends a blocked `wait_destroyed` and makes later ones return at once.
    fn cancel(&self);
}

/// Follows the app container's lifecycle through `docker events`.
#[derive(Default)]
struct DockerEvents {
    cancelled: AtomicBool,
    listener: Mutex<Option<Child>>,
}

impl DockerEvents {
    fn stop_listener(&self) {
        let listener = self
            .listener
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(mut child) = listener {
            // `kill` fails only when the stream already ended; `wait` reaps the process either way.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl ContainerEvents for DockerEvents {
    fn identify(&self, name: &str) -> Result<Option<WatchedContainer>> {
        if !container_exists(name)? {
            return Ok(None);
        }
        let details = docker_output([
            "container",
            "inspect",
            "--format",
            "{{.Id}} {{.Created}}",
            name,
        ])?;
        let (id, created) = details
            .split_once(' ')
            .ok_or_else(|| anyhow!("Docker returned invalid container details: {details}"))?;
        Ok(Some(WatchedContainer {
            id: id.to_owned(),
            created: created.to_owned(),
        }))
    }

    fn exists(&self, id: &str) -> Result<bool> {
        docker_listed([
            "container",
            "ls",
            "--all",
            "--quiet",
            "--filter",
            &format!("id={id}"),
        ])
    }

    fn wait_destroyed(&self, container: &WatchedContainer) -> Result<bool> {
        let stdout = {
            let mut listener = self.listener.lock().unwrap_or_else(PoisonError::into_inner);
            // Checked under the lock so `cancel` either sees this listener or stops it starting.
            if self.cancelled.load(Ordering::SeqCst) {
                return Ok(false);
            }
            let mut child = docker_cli()
                .args([
                    "events",
                    "--since",
                    &container.created,
                    "--filter",
                    &format!("container={}", container.id),
                    "--filter",
                    "event=destroy",
                    "--format",
                    "{{.Action}}",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .context("running Docker")?;
            let stdout = child
                .stdout
                .take()
                .context("reading Docker container events")?;
            *listener = Some(child);
            stdout
        };
        let destroyed = BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .any(|action| action.trim() == "destroy");
        self.stop_listener();
        Ok(destroyed)
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.stop_listener();
    }
}

/// Returns whether the container named `name` was deleted, or `false` once `stop` disconnects.
/// A Docker error never counts as deletion: the state is unknown, so it retries after `retry`.
fn watch_deletion(
    events: &dyn ContainerEvents,
    name: &str,
    retry: Duration,
    stop: &mpsc::Receiver<()>,
) -> bool {
    let stopped = || !matches!(stop.recv_timeout(retry), Err(RecvTimeoutError::Timeout));
    let container = loop {
        match events.identify(name) {
            Ok(Some(container)) => break container,
            Ok(None) => return true,
            Err(_) if stopped() => return false,
            Err(_) => {}
        }
    };
    loop {
        if matches!(events.wait_destroyed(&container), Ok(true)) {
            return true;
        }
        if !matches!(stop.try_recv(), Err(TryRecvError::Empty)) {
            return false;
        }
        // The stream ended or failed, for example when the daemon restarted. Its buffered
        // events may be gone, so check the container itself before subscribing again.
        match events.exists(&container.id) {
            Ok(false) => return true,
            Ok(true) | Err(_) if stopped() => return false,
            Ok(true) | Err(_) => {}
        }
    }
}

/// Watches the app container on a background thread and reports its deletion to `Shutdown`.
/// Dropping it stops the watch and reaps its `docker events` process.
struct DeletionMonitor {
    events: Arc<dyn ContainerEvents>,
    stop: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl DeletionMonitor {
    fn spawn(
        events: Arc<dyn ContainerEvents>,
        app_container: &str,
        retry: Duration,
        shutdown: &Shutdown,
    ) -> Self {
        let (stop, stopped) = mpsc::channel();
        let watcher = events.clone();
        let name = app_container.to_owned();
        let notify = shutdown.sender.clone();
        let thread = thread::spawn(move || {
            if watch_deletion(watcher.as_ref(), &name, retry, &stopped) {
                // Fails only once `start` has stopped listening, when the notice no longer matters.
                let _ = notify.send(Ended::DeletedElsewhere);
            }
        });
        Self {
            events,
            stop: Some(stop),
            thread: Some(thread),
        }
    }
}

impl Drop for DeletionMonitor {
    fn drop(&mut self) {
        drop(self.stop.take());
        self.events.cancel();
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            eprintln!("deletion monitoring stopped unexpectedly");
        }
    }
}

trait StartHost {
    fn delete(&self, runtime: &Runtime, name: &InstanceName) -> Result<()>;
    fn allocate(
        &self,
        runtime: &Runtime,
        name: &InstanceName,
        shutdown: &Shutdown,
        port_offset: u16,
    ) -> Result<Endpoints>;
    fn wait_ready(
        &self,
        endpoints: &Endpoints,
        app_container: &str,
        timeout: Duration,
        shutdown: &Shutdown,
    ) -> Result<()>;
    fn open_url(&self, url: &str) -> Result<()>;
    fn wait_for_shutdown(&self, app_container: &str, shutdown: &Shutdown) -> Result<Ended>;
}

struct DockerHost;

impl StartHost for DockerHost {
    fn delete(&self, runtime: &Runtime, name: &InstanceName) -> Result<()> {
        runtime.delete_instance_resources(name)
    }

    fn allocate(
        &self,
        runtime: &Runtime,
        name: &InstanceName,
        shutdown: &Shutdown,
        port_offset: u16,
    ) -> Result<Endpoints> {
        fs::create_dir_all(runtime.instance_dir(name))?;
        let prefix = prefix(name);
        let ports = host_ports(port_offset)?;
        require_free_loopback(ports.dashboard)?;
        require_free_loopback(ports.rpc)?;
        require_free_loopback(ports.p2p)?;
        require_free_loopback(ports.lightwalletd)?;
        ensure_network(&prefix)?;
        shutdown.check()?;
        for suffix in ["chain", "wallet", "lightwalletd", "config"] {
            ensure_volume(&format!("{prefix}-{suffix}"), name)?;
        }
        shutdown.check()?;

        if !container_exists(&format!("{prefix}-init"))? {
            docker([
                "create",
                "--name",
                &format!("{prefix}-init"),
                "--label",
                &label(name),
                "-v",
                &format!("{prefix}-wallet:/data"),
                "-v",
                &format!("{prefix}-config:/config"),
                &app_image(),
                "init",
                "--data-dir",
                "/data",
                "--config-dir",
                "/config",
            ])?;
            shutdown.check()?;
            docker(["start", "-a", &format!("{prefix}-init")])?;
            shutdown.check()?;
        }

        ensure_zakura(&prefix, name, &ports)?;
        shutdown.check()?;
        ensure_lightwalletd(&prefix, name, &ports)?;
        shutdown.check()?;
        let zakura_container = format!("{prefix}-zakura");
        docker(["start", &zakura_container])?;
        shutdown.check()?;
        let zakura_rpc = format!(
            "http://127.0.0.1:{}",
            published_port(&zakura_container, "18232/tcp")?
        );
        wait_for_zakura_tip(
            &zakura_rpc,
            &zakura_container,
            Duration::from_secs(120),
            shutdown,
        )?;
        docker(["start", &format!("{prefix}-lightwalletd")])?;
        shutdown.check()?;
        ensure_app(&prefix, name, &ports)?;
        shutdown.check()?;
        docker(["start", &format!("{prefix}-app")])?;
        shutdown.check()?;
        let endpoints = endpoints_for(&ports);
        runtime.write_instance(name, &endpoints)?;
        Ok(endpoints)
    }

    fn wait_ready(
        &self,
        endpoints: &Endpoints,
        app_container: &str,
        timeout: Duration,
        shutdown: &Shutdown,
    ) -> Result<()> {
        wait_ready(&endpoints.dashboard, app_container, timeout, shutdown)
    }

    fn open_url(&self, url: &str) -> Result<()> {
        open_url(url)
    }

    fn wait_for_shutdown(&self, app_container: &str, shutdown: &Shutdown) -> Result<Ended> {
        let _monitor = DeletionMonitor::spawn(
            Arc::new(DockerEvents::default()),
            app_container,
            DELETION_WATCH_RETRY,
            shutdown,
        );
        shutdown.wait()
    }
}

struct CleanupOnDrop<'a> {
    runtime: &'a Runtime,
    name: &'a InstanceName,
    host: &'a dyn StartHost,
    active: bool,
}

impl Drop for CleanupOnDrop<'_> {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if let Err(error) = self.host.delete(self.runtime, self.name) {
            eprintln!("could not delete {}: {error:#}", self.name);
        }
    }
}

impl Runtime {
    fn start_with(
        &self,
        name: &InstanceName,
        no_open: bool,
        json: bool,
        port_offset: u16,
        host: &dyn StartHost,
        shutdown: &Shutdown,
    ) -> Result<()> {
        let mut cleanup = CleanupOnDrop {
            runtime: self,
            name,
            host,
            active: true,
        };
        println!("Preparing a fresh {name} environment…");
        host.delete(self, name)?;
        println!("Starting {name}…");
        let endpoints = host.allocate(self, name, shutdown, port_offset)?;
        shutdown.check()?;
        let app_container = format!("{}-app", prefix(name));
        host.wait_ready(
            &endpoints,
            &app_container,
            Duration::from_secs(120),
            shutdown,
        )?;
        if json {
            println!("{}", serde_json::to_string_pretty(&endpoints)?);
        } else {
            println!("\n{name} is ready 🌸\n{}", endpoint_lines(&endpoints));
        }
        if !no_open {
            host.open_url(&endpoints.dashboard)?;
        }
        if !json {
            println!("\nPress Ctrl+C to stop and delete this development environment.");
        }
        match host.wait_for_shutdown(&app_container, shutdown)? {
            Ended::Interrupted => {
                println!("\nStopping and deleting {name}…");
                host.delete(self, name)?;
                cleanup.active = false;
                println!("Deleted {name} and all of its development data.");
            }
            Ended::DeletedElsewhere => {
                cleanup.active = false;
                println!("\n{name} was stopped and deleted by another command.");
            }
        }
        Ok(())
    }
}

/// Lists only running containers, so a stopped or missing one is `false` rather than an error.
fn container_running(name: &str) -> Result<bool> {
    docker_listed([
        "container",
        "ls",
        "--quiet",
        "--filter",
        &format!("name=^{name}$"),
    ])
}
fn wait_ready(
    base: &str,
    app_container: &str,
    timeout: Duration,
    shutdown: &Shutdown,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        shutdown.check()?;
        if Command::new("curl")
            .args(["-fsS", &format!("{base}/api/v1/health")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
        {
            return Ok(());
        }
        if !container_running(app_container)? {
            let logs = docker_logs(app_container)
                .unwrap_or_else(|error| format!("could not read app logs: {error}"));
            bail!("app exited before becoming healthy:\n{logs}");
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        shutdown.wait_timeout(remaining.min(Duration::from_millis(750)))?;
    }
    bail!(
        "dashboard did not become healthy within {} seconds",
        timeout.as_secs()
    )
}
fn wait_for_zakura_tip(
    base: &str,
    container: &str,
    timeout: Duration,
    shutdown: &Shutdown,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        shutdown.check()?;
        let tip_available = Command::new("curl")
            .args([
                "-sS",
                "-H",
                "content-type: application/json",
                "--data",
                r#"{"jsonrpc":"2.0","id":1,"method":"getbestblockhash","params":[]}"#,
                base,
            ])
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| serde_json::from_slice::<serde_json::Value>(&output.stdout).ok())
            .and_then(|response| {
                response
                    .get("result")
                    .and_then(|result| result.as_str())
                    .map(str::to_owned)
            })
            .is_some();
        if tip_available {
            return Ok(());
        }
        if !container_running(container)? {
            let logs = docker_logs(container)
                .unwrap_or_else(|error| format!("could not read Zakura logs: {error}"));
            bail!("Zakura exited before its RPC tip became available:\n{logs}");
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        shutdown.wait_timeout(remaining.min(Duration::from_millis(250)))?;
    }
    bail!(
        "Zakura RPC tip did not become available within {} seconds",
        timeout.as_secs()
    )
}
fn status_text(name: &InstanceName, running: bool, e: &Endpoints) -> String {
    let state = if running { "running" } else { "stopped" };
    format!("{name}: {state}\n{}", endpoint_lines(e))
}
fn endpoint_lines(e: &Endpoints) -> String {
    format!(
        "  Dashboard    {}\n  Zakura RPC   {}\n  lightwalletd {}  (network={}, tls={})\n  P2P          {}",
        e.dashboard, e.rpc, e.lightwalletd, e.network, e.tls, e.p2p
    )
}
fn open_url(url: &str) -> Result<()> {
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else {
        ("xdg-open", vec![url])
    };
    Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("opening {url}"))?;
    Ok(())
}
fn docker_cli() -> Command {
    #[allow(unused_mut)]
    let mut command = Command::new("docker");
    #[cfg(test)]
    if tests::DOCKER_UNREACHABLE.get() {
        command.env("DOCKER_HOST", "unix:///nonexistent/ths-tests/docker.sock");
    }
    command
}
fn docker<const N: usize>(args: [&str; N]) -> Result<()> {
    docker_inherit(&args)
}
fn docker_inherit(args: &[&str]) -> Result<()> {
    docker_command(args, None)
}
fn docker_inherit_in(args: &[&str], current_dir: &std::path::Path) -> Result<()> {
    docker_command(args, Some(current_dir))
}
fn docker_command(args: &[&str], current_dir: Option<&std::path::Path>) -> Result<()> {
    let mut command = docker_cli();
    command.args(args);
    if let Some(current_dir) = current_dir {
        command.current_dir(current_dir);
    }
    let status = command.status().context("running Docker")?;
    if !status.success() {
        bail!("docker {} failed", args.join(" "));
    }
    Ok(())
}
fn docker_output<const N: usize>(args: [&str; N]) -> Result<String> {
    let output = docker_cli().args(args).output().context("running Docker")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}
fn docker_logs(container: &str) -> Result<String> {
    let output = docker_cli()
        .args(["logs", "--tail", "50", container])
        .output()
        .context("running Docker")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    let mut logs = output.stdout;
    logs.extend_from_slice(&output.stderr);
    Ok(String::from_utf8_lossy(&logs).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    thread_local! {
        /// Points this test thread's Docker commands at a daemon that does not exist.
        pub(super) static DOCKER_UNREACHABLE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    fn with_unreachable_docker<T>(test: impl FnOnce() -> T) -> T {
        DOCKER_UNREACHABLE.set(true);
        let result = test();
        DOCKER_UNREACHABLE.set(false);
        result
    }

    #[test]
    fn docker_errors_are_not_reported_as_missing_resources() {
        with_unreachable_docker(|| {
            assert!(container_exists("ths-alpha-app").is_err());
            assert!(container_running("ths-alpha-app").is_err());
            assert!(volume_exists("ths-alpha-chain").is_err());
            assert!(network_exists("ths-alpha").is_err());
            assert!(DockerEvents::default().identify("ths-alpha-app").is_err());
            assert!(DockerEvents::default().exists("0123abcd").is_err());
        });
    }

    #[test]
    fn deleting_keeps_metadata_while_docker_is_unreachable() {
        let runtime = Runtime {
            root: std::env::temp_dir().join(format!("ths-unreachable-{}", std::process::id())),
        };
        let name = name("alpha");
        fs::create_dir_all(runtime.instance_dir(&name)).unwrap();
        runtime
            .write_instance(&name, &endpoints_for(&host_ports(0).unwrap()))
            .unwrap();
        let err = with_unreachable_docker(|| runtime.stop(&name)).unwrap_err();
        let message = err.to_string();
        for resource in [
            "container ths-alpha-app",
            "volume ths-alpha-chain",
            "network ths-alpha",
        ] {
            assert!(
                message.contains(resource),
                "{resource} missing from {message}"
            );
        }
        assert!(runtime.read_instance(&name).is_ok(), "metadata was deleted");
        fs::remove_dir_all(&runtime.root).unwrap();
    }

    #[test]
    fn commands_report_docker_errors_instead_of_a_stopped_environment() {
        let runtime = runtime_for_tests();
        let name = name("alpha");
        let err = with_unreachable_docker(|| runtime.mine(&name, 1, false)).unwrap_err();
        assert!(
            !err.to_string().contains("is not running"),
            "reported a Docker error as a stopped environment: {err}"
        );
    }

    struct RecordingHost {
        events: Arc<Mutex<Vec<String>>>,
        wait_ready_result: Result<(), String>,
        open_url_result: Result<(), String>,
        interrupt_before_ready: bool,
        deleted_elsewhere: bool,
    }

    impl RecordingHost {
        fn new() -> (Self, Arc<Mutex<Vec<String>>>) {
            let events = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    events: events.clone(),
                    wait_ready_result: Ok(()),
                    open_url_result: Ok(()),
                    interrupt_before_ready: false,
                    deleted_elsewhere: false,
                },
                events,
            )
        }

        fn push(&self, event: &str) {
            self.events.lock().unwrap().push(event.to_owned());
        }
    }

    impl StartHost for RecordingHost {
        fn delete(&self, _runtime: &Runtime, name: &InstanceName) -> Result<()> {
            self.push(&format!("delete:{name}"));
            Ok(())
        }

        fn allocate(
            &self,
            _runtime: &Runtime,
            name: &InstanceName,
            shutdown: &Shutdown,
            _port_offset: u16,
        ) -> Result<Endpoints> {
            self.push(&format!("allocate:{name}"));
            shutdown.check()?;
            Ok(Endpoints {
                dashboard: "http://127.0.0.1:1".into(),
                rpc: "http://127.0.0.1:2".into(),
                lightwalletd: "http://127.0.0.1:3".into(),
                p2p: "127.0.0.1:4".into(),
                network: default_regtest(),
                tls: false,
            })
        }

        fn wait_ready(
            &self,
            _endpoints: &Endpoints,
            _app_container: &str,
            _timeout: Duration,
            shutdown: &Shutdown,
        ) -> Result<()> {
            self.push("wait_ready");
            if self.interrupt_before_ready {
                bail!("interrupted");
            }
            shutdown.check()?;
            self.wait_ready_result
                .as_ref()
                .map(|_| ())
                .map_err(|e| anyhow!("{e}"))
        }

        fn open_url(&self, url: &str) -> Result<()> {
            self.push(&format!("open_url:{url}"));
            self.open_url_result
                .as_ref()
                .map(|_| ())
                .map_err(|e| anyhow!("{e}"))
        }

        fn wait_for_shutdown(&self, app_container: &str, shutdown: &Shutdown) -> Result<Ended> {
            self.push(&format!("wait_for_shutdown:{app_container}"));
            let events = ScriptedEvents::default();
            let app = (!self.deleted_elsewhere).then(|| watched("app-1"));
            events.identify.lock().unwrap().push_back(Ok(app));
            let _monitor = DeletionMonitor::spawn(
                Arc::new(events),
                app_container,
                Duration::from_millis(1),
                shutdown,
            );
            shutdown.wait()
        }
    }

    fn runtime_for_tests() -> Runtime {
        Runtime {
            root: std::env::temp_dir().join("ths-start-cleanup-tests"),
        }
    }

    fn name(value: &str) -> InstanceName {
        value.parse().unwrap()
    }

    #[test]
    fn readiness_failure_deletes_the_started_instance() {
        let (mut host, events) = RecordingHost::new();
        host.wait_ready_result = Err("dashboard did not become healthy".into());
        let (_sender, shutdown) = Shutdown::channel();
        let err = runtime_for_tests()
            .start_with(&name("alpha"), false, false, 0, &host, &shutdown)
            .unwrap_err();
        assert!(err.to_string().contains("dashboard did not become healthy"));
        let events = events.lock().unwrap().clone();
        let allocate_pos = events
            .iter()
            .position(|e| e == "allocate:alpha")
            .expect("allocate:alpha");
        assert!(
            events[allocate_pos + 1..]
                .iter()
                .any(|e| e == "delete:alpha"),
            "expected delete:alpha after allocate:alpha, got {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| e.starts_with("delete:") && !e.ends_with("alpha"))
        );
        assert!(!events.iter().any(|e| e.starts_with("wait_for_shutdown")));
    }

    #[test]
    fn browser_open_failure_deletes_and_does_not_wait() {
        let (mut host, events) = RecordingHost::new();
        host.open_url_result = Err("opening http://127.0.0.1:1".into());
        let (_sender, shutdown) = Shutdown::channel();
        let err = runtime_for_tests()
            .start_with(&name("alpha"), false, false, 0, &host, &shutdown)
            .unwrap_err();
        assert!(err.to_string().contains("opening"));
        let events = events.lock().unwrap().clone();
        assert!(events.iter().any(|e| e.starts_with("open_url:")));
        assert!(events.iter().any(|e| e == "delete:alpha"));
        assert!(!events.iter().any(|e| e.starts_with("wait_for_shutdown")));
    }

    #[test]
    fn no_open_skips_browser_and_waits_for_shutdown() {
        let (host, events) = RecordingHost::new();
        let (sender, shutdown) = Shutdown::channel();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            sender.send(Ended::Interrupted).unwrap();
        });
        runtime_for_tests()
            .start_with(&name("alpha"), true, false, 0, &host, &shutdown)
            .unwrap();
        let events = events.lock().unwrap().clone();
        assert!(!events.iter().any(|e| e.starts_with("open_url:")));
        assert!(events.iter().any(|e| e.starts_with("wait_for_shutdown")));
        assert!(events.iter().any(|e| e == "delete:alpha"));
    }

    #[test]
    fn deletion_elsewhere_ends_start_without_deleting_again() {
        let (mut host, events) = RecordingHost::new();
        host.deleted_elsewhere = true;
        let (_sender, shutdown) = Shutdown::channel();
        runtime_for_tests()
            .start_with(&name("alpha"), true, false, 0, &host, &shutdown)
            .unwrap();
        let events = events.lock().unwrap().clone();
        let waited = events
            .iter()
            .position(|e| e == "wait_for_shutdown:ths-alpha-app")
            .expect("waited on the app container");
        assert!(
            !events[waited..].iter().any(|e| e.starts_with("delete:")),
            "deleted again after another command deleted it: {events:?}"
        );
    }

    #[test]
    fn interrupt_before_ready_deletes_the_started_instance() {
        let (mut host, events) = RecordingHost::new();
        host.interrupt_before_ready = true;
        let (_sender, shutdown) = Shutdown::channel();
        let err = runtime_for_tests()
            .start_with(&name("alpha"), false, false, 0, &host, &shutdown)
            .unwrap_err();
        assert!(err.to_string().contains("interrupted"));
        let events = events.lock().unwrap().clone();
        assert!(events.iter().any(|e| e == "allocate:alpha"));
        assert!(events.iter().any(|e| e == "delete:alpha"));
        assert!(!events.iter().any(|e| e.starts_with("wait_for_shutdown")));
    }

    #[test]
    fn interrupt_during_allocate_deletes_only_the_named_instance() {
        let (host, events) = RecordingHost::new();
        let (sender, shutdown) = Shutdown::channel();
        sender.send(Ended::Interrupted).unwrap();
        let err = runtime_for_tests()
            .start_with(&name("alpha"), false, false, 0, &host, &shutdown)
            .unwrap_err();
        assert!(err.to_string().contains("interrupted"));
        let events = events.lock().unwrap().clone();
        assert!(events.iter().any(|e| e == "delete:alpha"));
        assert!(!events.iter().any(|e| e.contains("beta")));
    }

    #[test]
    fn validates_instance_names() {
        for valid in ["default", "project-2", "a"] {
            assert!(valid.parse::<InstanceName>().is_ok());
        }
        for invalid in ["", "UPPER", "with space", "-start", "end-"] {
            assert!(invalid.parse::<InstanceName>().is_err());
        }
    }

    #[test]
    fn project_images_are_version_locked() {
        assert_eq!(
            app_image(),
            format!(
                "ghcr.io/zcashlabs/thus-spoke-zakura-app:{}",
                env!("CARGO_PKG_VERSION")
            )
        );
        assert_eq!(
            lightwalletd_image(),
            format!(
                "ghcr.io/zcashlabs/thus-spoke-zakura-lightwalletd:{}",
                env!("CARGO_PKG_VERSION")
            )
        );
    }

    #[test]
    fn shutdown_reports_interrupt_from_channel() {
        let (sender, shutdown) = Shutdown::channel();
        assert!(!shutdown.try_interrupted());
        sender.send(Ended::Interrupted).unwrap();
        assert!(shutdown.try_interrupted());
        assert!(shutdown.try_interrupted());
        assert_eq!(shutdown.wait().unwrap(), Ended::Interrupted);
    }

    #[test]
    fn shutdown_check_bails_when_latched() {
        let (sender, shutdown) = Shutdown::channel();
        shutdown.check().unwrap();
        sender.send(Ended::Interrupted).unwrap();
        let err = shutdown.check().unwrap_err();
        assert!(err.to_string().contains("interrupted"));
        let err = shutdown.check().unwrap_err();
        assert!(err.to_string().contains("interrupted"));
    }

    #[test]
    fn shutdown_wait_timeout_wakes_on_signal() {
        let (sender, shutdown) = Shutdown::channel();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            sender.send(Ended::Interrupted).unwrap();
        });
        let started = Instant::now();
        let err = shutdown.wait_timeout(Duration::from_secs(2)).unwrap_err();
        assert!(err.to_string().contains("interrupted"));
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn shutdown_wait_timeout_returns_on_idle() {
        let (_sender, shutdown) = Shutdown::channel();
        shutdown.wait_timeout(Duration::from_millis(20)).unwrap();
    }

    #[test]
    fn shutdown_wait_returns_after_signal() {
        let (sender, shutdown) = Shutdown::channel();
        sender.send(Ended::Interrupted).unwrap();
        assert_eq!(shutdown.wait().unwrap(), Ended::Interrupted);
    }

    #[test]
    fn shutdown_wait_returns_when_the_environment_is_gone() {
        let (sender, shutdown) = Shutdown::channel();
        sender.send(Ended::DeletedElsewhere).unwrap();
        assert_eq!(shutdown.wait().unwrap(), Ended::DeletedElsewhere);
        assert!(!shutdown.try_interrupted());
    }

    #[test]
    fn shutdown_wait_keeps_a_deletion_notice_that_arrived_first() {
        let (sender, shutdown) = Shutdown::channel();
        sender.send(Ended::DeletedElsewhere).unwrap();
        sender.send(Ended::Interrupted).unwrap();
        assert_eq!(shutdown.wait().unwrap(), Ended::DeletedElsewhere);
        assert!(shutdown.try_interrupted());
    }

    fn watched(id: &str) -> WatchedContainer {
        WatchedContainer {
            id: id.to_owned(),
            created: "2026-01-01T00:00:00Z".to_owned(),
        }
    }

    /// Scripted Docker answers for `DeletionMonitor`. An unscripted `identify` or `exists`
    /// fails like an unreachable daemon; an unscripted `wait_destroyed` blocks like a quiet
    /// event stream until cancelled.
    #[derive(Default)]
    struct ScriptedEvents {
        identify: Mutex<VecDeque<Result<Option<WatchedContainer>, String>>>,
        streams: Mutex<VecDeque<Result<bool, String>>>,
        exists: Mutex<VecDeque<Result<bool, String>>>,
        calls: Mutex<Vec<String>>,
        cancelled: AtomicBool,
    }

    impl ScriptedEvents {
        fn record(&self, call: String) {
            self.calls.lock().unwrap().push(call);
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn wait_for_calls(&self, count: usize) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.calls.lock().unwrap().len() < count {
                assert!(
                    Instant::now() < deadline,
                    "calls so far: {:?}",
                    self.calls()
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    fn scripted<T>(queue: &Mutex<VecDeque<Result<T, String>>>) -> Option<Result<T>> {
        queue
            .lock()
            .unwrap()
            .pop_front()
            .map(|answer| answer.map_err(|e| anyhow!("{e}")))
    }

    impl ContainerEvents for ScriptedEvents {
        fn identify(&self, name: &str) -> Result<Option<WatchedContainer>> {
            self.record(format!("identify:{name}"));
            scripted(&self.identify).unwrap_or_else(|| Err(anyhow!("Docker is unreachable")))
        }

        fn exists(&self, id: &str) -> Result<bool> {
            self.record(format!("exists:{id}"));
            scripted(&self.exists).unwrap_or_else(|| Err(anyhow!("Docker is unreachable")))
        }

        fn wait_destroyed(&self, container: &WatchedContainer) -> Result<bool> {
            self.record(format!("wait_destroyed:{}", container.id));
            if let Some(answer) = scripted(&self.streams) {
                return answer;
            }
            while !self.cancelled.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(false)
        }

        fn cancel(&self) {
            self.cancelled.store(true, Ordering::SeqCst);
        }
    }

    fn monitor(events: &Arc<ScriptedEvents>, shutdown: &Shutdown) -> DeletionMonitor {
        DeletionMonitor::spawn(
            events.clone(),
            "ths-alpha-app",
            Duration::from_millis(1),
            shutdown,
        )
    }

    #[test]
    fn deletion_monitor_reports_a_destroy_event() {
        let events = Arc::new(ScriptedEvents::default());
        events
            .identify
            .lock()
            .unwrap()
            .push_back(Ok(Some(watched("app-1"))));
        events.streams.lock().unwrap().push_back(Ok(true));
        let (_sender, shutdown) = Shutdown::channel();
        let _monitor = monitor(&events, &shutdown);
        assert_eq!(shutdown.wait().unwrap(), Ended::DeletedElsewhere);
        assert_eq!(
            events.calls(),
            ["identify:ths-alpha-app", "wait_destroyed:app-1"]
        );
    }

    #[test]
    fn deletion_monitor_reports_a_container_already_gone() {
        let events = Arc::new(ScriptedEvents::default());
        events.identify.lock().unwrap().push_back(Ok(None));
        let (_sender, shutdown) = Shutdown::channel();
        let _monitor = monitor(&events, &shutdown);
        assert_eq!(shutdown.wait().unwrap(), Ended::DeletedElsewhere);
    }

    #[test]
    fn deletion_monitor_treats_docker_errors_as_unknown() {
        let events = Arc::new(ScriptedEvents::default());
        events.identify.lock().unwrap().extend([
            Err("Docker is unreachable".to_owned()),
            Ok(Some(watched("app-1"))),
        ]);
        events
            .streams
            .lock()
            .unwrap()
            .extend([Err("Docker is unreachable".to_owned()), Ok(false)]);
        let (sender, shutdown) = Shutdown::channel();
        let monitor = monitor(&events, &shutdown);
        // Both reconciliations fail like an unreachable daemon; the third stream stays open.
        events.wait_for_calls(7);
        sender.send(Ended::Interrupted).unwrap();
        assert_eq!(shutdown.wait().unwrap(), Ended::Interrupted);
        drop(monitor);
        assert_eq!(
            events.calls(),
            [
                "identify:ths-alpha-app",
                "identify:ths-alpha-app",
                "wait_destroyed:app-1",
                "exists:app-1",
                "wait_destroyed:app-1",
                "exists:app-1",
                "wait_destroyed:app-1",
            ]
        );
        assert!(shutdown.receiver.try_recv().is_err(), "reported a deletion");
    }

    #[test]
    fn deletion_monitor_resubscribes_after_a_disconnect() {
        let events = Arc::new(ScriptedEvents::default());
        events
            .identify
            .lock()
            .unwrap()
            .push_back(Ok(Some(watched("app-1"))));
        events.streams.lock().unwrap().extend([Ok(false), Ok(true)]);
        events.exists.lock().unwrap().push_back(Ok(true));
        let (_sender, shutdown) = Shutdown::channel();
        let _monitor = monitor(&events, &shutdown);
        assert_eq!(shutdown.wait().unwrap(), Ended::DeletedElsewhere);
        assert_eq!(
            events.calls(),
            [
                "identify:ths-alpha-app",
                "wait_destroyed:app-1",
                "exists:app-1",
                "wait_destroyed:app-1",
            ]
        );
    }

    #[test]
    fn deletion_monitor_follows_the_original_container_not_its_name() {
        // A disconnect hides the destroy event, and a new container already took the name.
        let events = Arc::new(ScriptedEvents::default());
        events
            .identify
            .lock()
            .unwrap()
            .extend([Ok(Some(watched("app-1"))), Ok(Some(watched("app-2")))]);
        events.streams.lock().unwrap().push_back(Ok(false));
        events.exists.lock().unwrap().push_back(Ok(false));
        let (_sender, shutdown) = Shutdown::channel();
        let _monitor = monitor(&events, &shutdown);
        assert_eq!(shutdown.wait().unwrap(), Ended::DeletedElsewhere);
        assert_eq!(
            events.calls(),
            [
                "identify:ths-alpha-app",
                "wait_destroyed:app-1",
                "exists:app-1",
            ]
        );
    }

    #[test]
    fn dropping_the_deletion_monitor_stops_the_watch() {
        let events = Arc::new(ScriptedEvents::default());
        events
            .identify
            .lock()
            .unwrap()
            .push_back(Ok(Some(watched("app-1"))));
        let (_sender, shutdown) = Shutdown::channel();
        let monitor = monitor(&events, &shutdown);
        events.wait_for_calls(2);
        drop(monitor);
        assert!(events.cancelled.load(Ordering::SeqCst));
        assert!(shutdown.receiver.try_recv().is_err(), "reported a deletion");
    }

    #[test]
    fn wait_ready_aborts_when_shutdown_is_signaled() {
        let (sender, shutdown) = Shutdown::channel();
        sender.send(Ended::Interrupted).unwrap();
        let err = wait_ready(
            "http://127.0.0.1:1",
            "missing-app",
            Duration::from_secs(5),
            &shutdown,
        )
        .unwrap_err();
        assert!(err.to_string().contains("interrupted"));
    }

    #[test]
    fn wait_for_zakura_tip_aborts_when_shutdown_is_signaled() {
        let (sender, shutdown) = Shutdown::channel();
        sender.send(Ended::Interrupted).unwrap();
        let err = wait_for_zakura_tip(
            "http://127.0.0.1:1",
            "missing-zakura",
            Duration::from_secs(5),
            &shutdown,
        )
        .unwrap_err();
        assert!(err.to_string().contains("interrupted"));
    }

    #[test]
    fn default_offset_uses_stable_loopback_ports() {
        let ports = host_ports(0).unwrap();
        assert_eq!(ports.dashboard, 32805);
        assert_eq!(ports.rpc, 18232);
        assert_eq!(ports.p2p, 18233);
        assert_eq!(ports.lightwalletd, 9067);
        assert_eq!(loopback_publish(ports.rpc, 18232), "127.0.0.1:18232:18232");
        assert_eq!(
            loopback_publish(ports.dashboard, 8080),
            "127.0.0.1:32805:8080"
        );
    }

    #[test]
    fn port_offset_shifts_all_four_hosts_by_the_same_stride() {
        let base = host_ports(0).unwrap();
        let shifted = host_ports(10).unwrap();
        assert_eq!(shifted.dashboard, base.dashboard + 10);
        assert_eq!(shifted.rpc, base.rpc + 10);
        assert_eq!(shifted.p2p, base.p2p + 10);
        assert_eq!(shifted.lightwalletd, base.lightwalletd + 10);
    }

    #[test]
    fn port_offset_rejects_values_that_are_not_multiples_of_ten() {
        let err = host_ports(1).unwrap_err();
        assert!(err.to_string().contains('1'));
    }

    #[test]
    fn reports_the_conflicting_loopback_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let err = require_free_loopback(port).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains(&port.to_string()),
            "error should name port {port}, got {message}"
        );
    }

    #[test]
    fn endpoints_json_includes_regtest_and_plaintext_lightwalletd() {
        let json = serde_json::to_value(endpoints_for(&host_ports(0).unwrap())).unwrap();
        assert_eq!(json["dashboard"], "http://127.0.0.1:32805");
        assert_eq!(json["rpc"], "http://127.0.0.1:18232");
        assert_eq!(json["lightwalletd"], "http://127.0.0.1:9067");
        assert_eq!(json["p2p"], "127.0.0.1:18233");
        assert_eq!(json["network"], "regtest");
        assert_eq!(json["tls"], false);
    }

    #[test]
    fn endpoints_json_without_network_fields_still_deserializes() {
        let parsed: Endpoints = serde_json::from_str(
            r#"{"dashboard":"http://127.0.0.1:1","rpc":"http://127.0.0.1:2","lightwalletd":"http://127.0.0.1:3","p2p":"127.0.0.1:4"}"#,
        )
        .unwrap();
        assert_eq!(parsed.network, "regtest");
        assert!(!parsed.tls);
    }
}
