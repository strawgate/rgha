//! Firecracker backend: one microVM per job on a KVM host.
//!
//! - **Rootfs**: built once from the runner image (+ `preload` layers + the
//!   guest init) with Docker, converted to ext4, cached by content hash, and
//!   shared **read-only** by every VM. Each VM gets a sparse scratch disk;
//!   the guest init overlays it on the rootfs and pivots into the overlay.
//! - **Fast boot**: per (vCPUs, memory) a template VM is booted until it
//!   waits for its config, then memory-snapshotted. Jobs restore the snapshot
//!   (VMGenID reseeds the guest RNG; `clock_realtime` fixes the clock) and
//!   find their config on the config drive. No per-job identity exists in
//!   the snapshot. If a restore fails, the VM cold-boots instead.
//! - **Config**: the single-use JIT config is written to a small read-only
//!   raw drive (`/dev/vdb`), never the kernel command line.
//! - **Network**: each VM runs in its own network namespace with an
//!   identical internal tap/guest address (required by snapshot restore),
//!   NATed onto a unique veth /30, then NATed out of the uplink. Guests can't
//!   reach the host, private ranges, or link-local/metadata addresses
//!   (`RGHA-FWD` chain, inserted into `DOCKER-USER` when Docker is present).
//! - **Isolation**: KVM; optionally the Firecracker jailer (chroot,
//!   unprivileged uid, its own /dev/kvm, the VM's netns).
//! - **Lifecycle**: the guest powers off when the runner exits; Firecracker
//!   exits; rgha tears down the namespace and disks. VM state lives under
//!   `state_dir/vms` so a restarted controller can reconcile.
//!
//! Requires root (namespaces, iptables, jailer) and `/dev/kvm`.

use std::collections::{BTreeSet, HashMap};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::sync::{OnceCell, watch};

use super::{Backend, Instance, Network, RunnerSpec};
use crate::egress::{EgressProxy, HTTP_PORT, TLS_PORT};
use crate::image::Preload;

pub const GUEST_INIT: &str = include_str!("../../assets/firecracker/rgha-init");
const MAX_SLOTS: u32 = 16_384; // /30 per VM inside a /16
/// Fixed size of the config drive: its size is part of a snapshot.
const CONFIG_DRIVE_BYTES: usize = 64 * 1024;
/// Inside every VM's namespace (identical for all, as snapshots require).
const TAP_HOST_IP: &str = "172.30.0.1";
const GUEST_IP: &str = "172.30.0.2";
const GUEST_MAC: &str = "06:00:ac:1e:00:02";

#[derive(Debug, Clone)]
pub struct JailerSettings {
    pub bin: String,
    pub uid: u32,
    pub gid: u32,
}

#[derive(Debug, Clone)]
pub struct FirecrackerSettings {
    pub image: String,
    pub image_commands: Vec<String>,
    pub preload: Preload,
    pub firecracker_bin: String,
    pub jailer: Option<JailerSettings>,
    pub kernel: String,
    pub state_dir: PathBuf,
    pub docker_bin: String,
    pub rootfs_size_gib: u32,
    pub scratch_size_gib: u32,
    /// Restore jobs from memory snapshots (fast boot). Off = cold boot.
    pub snapshots: bool,
    /// First two octets of the veth /16, e.g. `[10, 213]`.
    pub subnet: [u8; 2],
    pub uplink: Option<String>,
    pub dns: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Meta {
    id: String,
    class: String,
    runner_name: String,
    slot: u32,
    pid: u32,
    jailed: bool,
    /// Egress allowlist (domains, CIDRs) when the class restricts network.
    #[serde(default)]
    allow: Option<(Vec<String>, Vec<String>)>,
}

struct Vm {
    meta: Meta,
    exit: watch::Receiver<Option<i32>>,
    /// Started by a previous controller process (no waiter task).
    adopted: bool,
}

#[derive(Debug, Clone)]
struct Template {
    dir: PathBuf,
}

pub struct FirecrackerBackend(Arc<Inner>);

struct Inner {
    s: FirecrackerSettings,
    rootfs: OnceCell<PathBuf>,
    scratch: OnceCell<PathBuf>,
    uplink: OnceCell<String>,
    templates: tokio::sync::Mutex<HashMap<(u32, u32), Option<Template>>>,
    vms: Mutex<HashMap<String, Vm>>,
    slots: Mutex<BTreeSet<u32>>,
    egress: EgressProxy,
    egress_started: OnceCell<()>,
}

// ------------------------------------------------------------- pure helpers

/// iptables rules (table, chain, args) that lock a slot's VM down to: TCP
/// 443/80 via the egress proxy, DNS to `dns`, and `cidrs`. Applied with `-I`
/// (so they precede the general rules) and removed with `-D`.
pub(crate) fn egress_rules(slot: u32, dns: &str, cidrs: &[String]) -> Vec<(&'static str, &'static str, Vec<String>)> {
    let veth = veth_name(slot);
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let (tls, http) = (TLS_PORT.to_string(), HTTP_PORT.to_string());
    let mut rules = vec![
        ("nat", "RGHA-PRE", s(&["-i", &veth, "-p", "tcp", "--dport", "443", "-j", "REDIRECT", "--to-ports", &tls])),
        ("nat", "RGHA-PRE", s(&["-i", &veth, "-p", "tcp", "--dport", "80", "-j", "REDIRECT", "--to-ports", &http])),
        // Inserted in this order with -I, so the final DROP ends up last.
        ("filter", "RGHA-FWD", s(&["-i", &veth, "-j", "DROP"])),
    ];
    for c in cidrs {
        rules.push(("filter", "RGHA-FWD", s(&["-i", &veth, "-d", c, "-j", "ACCEPT"])));
    }
    for proto in ["udp", "tcp"] {
        rules.push(("filter", "RGHA-FWD", s(&["-i", &veth, "-p", proto, "-d", dns, "--dport", "53", "-j", "ACCEPT"])));
    }
    rules
}

/// Host and namespace ends of a slot's veth /30.
pub(crate) fn slot_addrs(subnet: [u8; 2], slot: u32) -> (Ipv4Addr, Ipv4Addr) {
    let base = u32::from(Ipv4Addr::new(subnet[0], subnet[1], 0, 0)) + slot * 4;
    (Ipv4Addr::from(base + 1), Ipv4Addr::from(base + 2))
}

pub(crate) fn netns_name(slot: u32) -> String {
    format!("rgha-{slot}")
}

pub(crate) fn veth_name(slot: u32) -> String {
    format!("rgha-v{slot}")
}

pub(crate) fn boot_args(dns: &str) -> String {
    format!(
        "console=ttyS0 reboot=k panic=1 pci=off quiet root=/dev/vda ro init=/sbin/rgha-init \
         ip={GUEST_IP}::{TAP_HOST_IP}:255.255.255.252::eth0:off rgha.dns={dns}"
    )
}

/// Config drive contents: `rgha1 <epoch>\n<jit>\n`, NUL padded to a fixed size.
pub(crate) fn config_drive(jit: &str, epoch_secs: f64) -> anyhow::Result<Vec<u8>> {
    let mut b = format!("rgha1 {epoch_secs:.3}\n{jit}\n").into_bytes();
    if b.len() > CONFIG_DRIVE_BYTES {
        bail!("JIT config too large for the config drive ({} bytes)", b.len());
    }
    b.resize(CONFIG_DRIVE_BYTES, 0);
    Ok(b)
}

pub(crate) fn vm_config(boot_args: &str, vcpus: u32, mem_mib: u32, kernel: &str) -> serde_json::Value {
    serde_json::json!({
        "boot-source": { "kernel_image_path": kernel, "boot_args": boot_args },
        "drives": [
            { "drive_id": "rootfs", "path_on_host": "rootfs.ext4", "is_root_device": true, "is_read_only": true },
            { "drive_id": "config", "path_on_host": "config.img", "is_root_device": false, "is_read_only": true },
            { "drive_id": "scratch", "path_on_host": "scratch.ext4", "is_root_device": false, "is_read_only": false },
        ],
        "machine-config": { "vcpu_count": vcpus, "mem_size_mib": mem_mib, "smt": false },
        "network-interfaces": [ { "iface_id": "eth0", "guest_mac": GUEST_MAC, "host_dev_name": "tap0" } ],
        "entropy": {},
    })
}

/// Dockerfile for the rootfs: runner image + preloads + user commands + init.
pub(crate) fn dockerfile(s: &FirecrackerSettings) -> String {
    let mut lines = vec![format!("FROM {}", s.image)];
    lines.extend(s.preload.dockerfile_commands());
    lines.extend(s.image_commands.iter().cloned());
    lines.push("USER root".into());
    lines.push("COPY rgha-init /sbin/rgha-init".into());
    lines.push("RUN chmod 0755 /sbin/rgha-init && mkdir -p /mnt/scratch /mnt/newroot".into());
    // Docker ENV doesn't survive `docker export`; record it for the guest init.
    lines.push(
        "RUN mkdir -p /etc/rgha && env | grep -Ev '^(HOSTNAME|HOME|PWD|SHLVL|_|OLDPWD)=' > /etc/rgha/image.env".into(),
    );
    lines.join("\n") + "\n"
}

pub(crate) fn content_hash(s: &str) -> String {
    // FNV-1a 64: stable, dependency-free, good enough for a cache key.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

// ------------------------------------------------------------- process helpers

async fn run(cmd: &str, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new(cmd).args(args).output().await.with_context(|| format!("running {cmd}"))?;
    if !out.status.success() {
        bail!("{cmd} {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Runs and ignores failure (idempotent setup/teardown).
async fn run_ok(cmd: &str, args: &[&str]) {
    let _ = Command::new(cmd).args(args).stdout(Stdio::null()).stderr(Stdio::null()).status().await;
}

fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Calls the Firecracker API on a unix socket (via curl, which every host has).
async fn fc_api(sock: &Path, method: &str, path: &str, body: &serde_json::Value) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !sock.exists() {
        if Instant::now() > deadline {
            bail!("firecracker API socket {} did not appear", sock.display());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    run(
        "curl",
        &[
            "-sS",
            "--fail-with-body",
            "--unix-socket",
            sock.to_str().unwrap(),
            "-X",
            method,
            &format!("http://localhost{path}"),
            "-H",
            "Content-Type: application/json",
            "-d",
            &body.to_string(),
        ],
    )
    .await
    .map(|_| ())
}

async fn link_or_copy(src: &Path, dst: &Path) -> anyhow::Result<()> {
    if tokio::fs::hard_link(src, dst).await.is_err() {
        tokio::fs::copy(src, dst).await?;
    }
    Ok(())
}

impl FirecrackerBackend {
    pub fn new(s: FirecrackerSettings) -> Self {
        Self(Arc::new(Inner {
            s,
            rootfs: OnceCell::new(),
            scratch: OnceCell::new(),
            uplink: OnceCell::new(),
            templates: tokio::sync::Mutex::new(HashMap::new()),
            vms: Mutex::new(HashMap::new()),
            slots: Mutex::new(BTreeSet::new()),
            egress: EgressProxy::default(),
            egress_started: OnceCell::new(),
        }))
    }
}

#[async_trait]
impl Backend for FirecrackerBackend {
    fn kind(&self) -> &'static str {
        "firecracker"
    }
    async fn prepare(&self) -> anyhow::Result<()> {
        self.0.do_prepare().await
    }
    async fn start(&self, spec: &RunnerSpec) -> anyhow::Result<String> {
        self.0.do_start(spec).await
    }
    async fn stop(&self, id: &str) -> anyhow::Result<()> {
        self.0.do_stop(id).await
    }
    async fn wait(&self, id: &str) -> anyhow::Result<Option<i32>> {
        self.0.do_wait(id).await
    }
    async fn list(&self, class: &str) -> anyhow::Result<Vec<Instance>> {
        self.0.do_list(class).await
    }
}

/// Where a VM's files live and how Firecracker is launched for it.
struct Layout {
    /// Directory holding the VM's files; Firecracker's cwd (or chroot root).
    files: PathBuf,
    /// Kernel path as Firecracker sees it.
    kernel: String,
    jail: Option<JailerSettings>,
}

impl Inner {
    fn vms_dir(&self) -> PathBuf {
        self.s.state_dir.join("vms")
    }

    fn jail_dir(&self, id: &str) -> PathBuf {
        self.s.state_dir.join("jail").join("firecracker").join(id)
    }

    async fn uplink(&self) -> anyhow::Result<&String> {
        self.uplink
            .get_or_try_init(|| async {
                if let Some(u) = &self.s.uplink {
                    return Ok(u.clone());
                }
                let route = run("ip", &["route", "show", "default"]).await?;
                route
                    .split_whitespace()
                    .skip_while(|w| *w != "dev")
                    .nth(1)
                    .map(str::to_string)
                    .context("no default route; set `uplink`")
            })
            .await
    }

    /// Idempotent host network setup in dedicated chains.
    async fn setup_network(&self) -> anyhow::Result<()> {
        let up = self.uplink().await?.clone();
        let [a, b] = self.s.subnet;
        let subnet = format!("{a}.{b}.0.0/16");
        run_ok("sysctl", &["-qw", "net.ipv4.ip_forward=1"]).await;
        run_ok("iptables", &["-t", "nat", "-N", "RGHA-NAT"]).await;
        if run("iptables", &["-t", "nat", "-C", "POSTROUTING", "-j", "RGHA-NAT"]).await.is_err() {
            run("iptables", &["-t", "nat", "-A", "POSTROUTING", "-j", "RGHA-NAT"]).await?;
        }
        run("iptables", &["-t", "nat", "-F", "RGHA-NAT"]).await?;
        run("iptables", &["-t", "nat", "-A", "RGHA-NAT", "-s", &subnet, "-o", &up, "-j", "MASQUERADE"]).await?;

        run_ok("iptables", &["-N", "RGHA-FWD"]).await;
        run("iptables", &["-F", "RGHA-FWD"]).await?;
        let mut rules: Vec<Vec<String>> = vec![];
        let r = |args: &[&str]| args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        rules.push(r(&["-o", "rgha+", "-m", "conntrack", "--ctstate", "ESTABLISHED,RELATED", "-j", "ACCEPT"]));
        for net in ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "169.254.0.0/16", "100.64.0.0/10"] {
            rules.push(r(&["-i", "rgha+", "-d", net, "-j", "DROP"]));
        }
        rules.push(r(&["-i", "rgha+", "-o", &up, "-j", "ACCEPT"]));
        rules.push(r(&["-i", "rgha+", "-j", "DROP"]));
        for rule in rules {
            let mut args = vec!["-A", "RGHA-FWD"];
            args.extend(rule.iter().map(String::as_str));
            run("iptables", &args).await?;
        }
        // Docker's FORWARD policy is DROP; its DOCKER-USER chain runs first.
        let hook = if run("iptables", &["-L", "DOCKER-USER", "-n"]).await.is_ok() { "DOCKER-USER" } else { "FORWARD" };
        if run("iptables", &["-C", hook, "-j", "RGHA-FWD"]).await.is_err() {
            run("iptables", &["-I", hook, "-j", "RGHA-FWD"]).await?;
        }
        // Guests can't talk to the host itself, except the egress proxy ports
        // that locked-down VMs are redirected to (ACCEPT inserted above DROP).
        if run("iptables", &["-C", "INPUT", "-i", "rgha+", "-j", "DROP"]).await.is_err() {
            run("iptables", &["-I", "INPUT", "-i", "rgha+", "-j", "DROP"]).await?;
        }
        let ports = format!("{TLS_PORT},{HTTP_PORT}");
        let accept = ["INPUT", "-i", "rgha+", "-p", "tcp", "-m", "multiport", "--dports", &ports, "-j", "ACCEPT"];
        if run("iptables", &[&["-C"][..], &accept[..]].concat()).await.is_err() {
            run("iptables", &[&["-I"][..], &accept[..]].concat()).await?;
        }
        run_ok("iptables", &["-t", "nat", "-N", "RGHA-PRE"]).await;
        if run("iptables", &["-t", "nat", "-C", "PREROUTING", "-j", "RGHA-PRE"]).await.is_err() {
            run("iptables", &["-t", "nat", "-I", "PREROUTING", "-j", "RGHA-PRE"]).await?;
        }
        Ok(())
    }

    /// Locks a slot down to its allowlist (rules + proxy registration).
    async fn apply_egress(&self, slot: u32, domains: &[String], cidrs: &[String]) -> anyhow::Result<()> {
        self.egress_started.get_or_try_init(|| self.egress.start()).await?;
        self.egress.register(slot_addrs(self.s.subnet, slot).1, domains.to_vec());
        for (table, chain, rule) in egress_rules(slot, &self.s.dns, cidrs) {
            let mut args = vec!["-t", table, "-I", chain];
            args.extend(rule.iter().map(String::as_str));
            run("iptables", &args).await?;
        }
        Ok(())
    }

    async fn remove_egress(&self, slot: u32, cidrs: &[String]) {
        self.egress.unregister(slot_addrs(self.s.subnet, slot).1);
        for (table, chain, rule) in egress_rules(slot, &self.s.dns, cidrs) {
            let mut args = vec!["-t", table, "-D", chain];
            args.extend(rule.iter().map(String::as_str));
            run_ok("iptables", &args).await;
        }
    }

    /// Per-VM namespace: tap0 (identical in every VM) NATed onto a unique veth.
    async fn setup_netns(&self, slot: u32, tap_owner: Option<u32>) -> anyhow::Result<()> {
        let ns = netns_name(slot);
        let veth = veth_name(slot);
        let (host, inner) = slot_addrs(self.s.subnet, slot);
        run_ok("ip", &["netns", "del", &ns]).await;
        run("ip", &["netns", "add", &ns]).await?;
        run("ip", &["link", "add", &veth, "type", "veth", "peer", "name", "veth0", "netns", &ns]).await?;
        run("ip", &["addr", "add", &format!("{host}/30"), "dev", &veth]).await?;
        run("ip", &["link", "set", &veth, "up"]).await?;
        let e = |args: &[&str]| {
            let mut v = vec!["netns".to_string(), "exec".into(), ns.clone()];
            v.extend(args.iter().map(|s| s.to_string()));
            v
        };
        let mut tap = vec!["ip", "tuntap", "add", "tap0", "mode", "tap"];
        let owner = tap_owner.map(|u| u.to_string());
        if let Some(o) = &owner {
            tap.extend(["user", o.as_str()]);
        }
        let inner_cidr = format!("{inner}/30");
        let tap_cidr = format!("{TAP_HOST_IP}/30");
        let host_s = host.to_string();
        for cmd in [
            e(&["ip", "link", "set", "lo", "up"]),
            e(&["ip", "addr", "add", &inner_cidr, "dev", "veth0"]),
            e(&["ip", "link", "set", "veth0", "up"]),
            e(&["ip", "route", "add", "default", "via", &host_s]),
            e(&tap),
            e(&["ip", "addr", "add", &tap_cidr, "dev", "tap0"]),
            e(&["ip", "link", "set", "tap0", "up"]),
            e(&["sysctl", "-qw", "net.ipv4.ip_forward=1"]),
            e(&["iptables", "-t", "nat", "-A", "POSTROUTING", "-o", "veth0", "-j", "MASQUERADE"]),
        ] {
            let args: Vec<&str> = cmd.iter().map(String::as_str).collect();
            run("ip", &args).await?;
        }
        Ok(())
    }

    async fn build_rootfs(&self) -> anyhow::Result<PathBuf> {
        let dockerfile = dockerfile(&self.s);
        let hash = content_hash(&format!("{dockerfile}\n{GUEST_INIT}\n{}", self.s.rootfs_size_gib));
        let out = self.s.state_dir.join(format!("rootfs-{hash}.ext4"));
        if out.exists() {
            return Ok(out);
        }
        let build = self.s.state_dir.join(format!("build-{hash}"));
        tokio::fs::create_dir_all(&build).await?;
        tokio::fs::write(build.join("Dockerfile"), &dockerfile).await?;
        tokio::fs::write(build.join("rgha-init"), GUEST_INIT).await?;
        let tag = format!("rgha-fc:{hash}");
        tracing::info!(image = %self.s.image, %tag, "building Firecracker rootfs");
        run(&self.s.docker_bin, &["build", "-q", "-t", &tag, build.to_str().unwrap()]).await?;
        let container = run(&self.s.docker_bin, &["create", &tag]).await?;

        // docker export | (helper) mkfs.ext4 -d: preserves ownership without host root tricks.
        let docker = self.s.docker_bin.clone();
        let state = self.s.state_dir.clone();
        let tmp_name = format!("rootfs-{hash}.ext4.tmp");
        let size = self.s.rootfs_size_gib;
        let cid = container.clone();
        let tmp_in = tmp_name.clone();
        let res = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let tmp_name = tmp_in;
            let mut export =
                std::process::Command::new(&docker).args(["export", &cid]).stdout(Stdio::piped()).spawn()?;
            let script = format!(
                "apk add -q e2fsprogs >/dev/null && mkdir /r && tar -x -C /r && rm -f /out/{tmp_name} && \
                 truncate -s {size}G /out/{tmp_name} && mkfs.ext4 -q -F -L rootfs -d /r /out/{tmp_name}"
            );
            let status = std::process::Command::new(&docker)
                .args([
                    "run",
                    "-i",
                    "--rm",
                    "-v",
                    &format!("{}:/out", state.display()),
                    "alpine:3.20",
                    "sh",
                    "-c",
                    &script,
                ])
                .stdin(export.stdout.take().context("export stdout")?)
                .status()?;
            let export_status = export.wait()?;
            if !status.success() || !export_status.success() {
                bail!("rootfs conversion failed (export {export_status}, mkfs {status})");
            }
            Ok(())
        })
        .await?;
        run_ok(&self.s.docker_bin, &["rm", "-f", &container]).await;
        res?;
        tokio::fs::rename(self.s.state_dir.join(&tmp_name), &out).await?;
        run("chmod", &["0644", out.to_str().unwrap()]).await?;
        let _ = tokio::fs::remove_dir_all(&build).await;
        tracing::info!(rootfs = %out.display(), "Firecracker rootfs ready");
        Ok(out)
    }

    /// An empty, sparse ext4 scratch disk to copy per VM.
    async fn build_scratch(&self) -> anyhow::Result<PathBuf> {
        let out = self.s.state_dir.join(format!("scratch-{}g.ext4", self.s.scratch_size_gib));
        if !out.exists() {
            let tmp = out.with_extension("tmp");
            run("truncate", &["-s", &format!("{}G", self.s.scratch_size_gib), tmp.to_str().unwrap()]).await?;
            run(
                "mkfs.ext4",
                &["-q", "-F", "-L", "scratch", "-E", "lazy_itable_init=1,lazy_journal_init=1", tmp.to_str().unwrap()],
            )
            .await?;
            tokio::fs::rename(&tmp, &out).await?;
        }
        Ok(out)
    }

    fn alloc_slot(&self) -> anyhow::Result<u32> {
        let mut slots = self.slots.lock().expect("slots lock");
        let slot = (0..MAX_SLOTS).find(|s| !slots.contains(s)).context("no free VM network slots")?;
        slots.insert(slot);
        Ok(slot)
    }

    fn free_slot(&self, slot: u32) {
        self.slots.lock().expect("slots lock").remove(&slot);
    }

    async fn teardown(&self, id: &str, slot: u32) {
        run_ok("ip", &["netns", "del", &netns_name(slot)]).await;
        run_ok("ip", &["link", "del", &veth_name(slot)]).await;
        let _ = tokio::fs::remove_dir_all(self.vms_dir().join(id)).await;
        let _ = tokio::fs::remove_dir_all(self.jail_dir(id)).await;
        self.free_slot(slot);
    }

    async fn cleanup(&self, meta: &Meta) {
        if let Some((_, cidrs)) = &meta.allow {
            self.remove_egress(meta.slot, cidrs).await;
        }
        self.teardown(&meta.id, meta.slot).await;
    }

    /// Cleans up adopted VMs (no waiter task) whose Firecracker process has
    /// exited since adoption. Called on every reconcile via `list`.
    async fn sweep_dead_adopted(&self) {
        let dead: Vec<Meta> = {
            let mut vms = self.vms.lock().expect("vms lock");
            let ids: Vec<String> =
                vms.iter().filter(|(_, v)| v.adopted && !pid_alive(v.meta.pid)).map(|(id, _)| id.clone()).collect();
            ids.into_iter().filter_map(|id| vms.remove(&id)).map(|v| v.meta).collect()
        };
        for meta in dead {
            tracing::info!(vm = %meta.id, "cleaning up exited VM adopted from a previous controller");
            self.cleanup(&meta).await;
        }
    }

    /// Adopts VMs left by a previous controller process: live ones are
    /// tracked (so reconcile can decide), dead ones are cleaned up.
    async fn adopt_existing(&self) -> anyhow::Result<()> {
        let mut dir = match tokio::fs::read_dir(self.vms_dir()).await {
            Ok(d) => d,
            Err(_) => return Ok(()),
        };
        while let Some(e) = dir.next_entry().await? {
            let Ok(text) = tokio::fs::read_to_string(e.path().join("meta.json")).await else {
                // Template builds or half-started VMs: no meta, nothing running.
                let _ = tokio::fs::remove_dir_all(e.path()).await;
                continue;
            };
            let Ok(meta) = serde_json::from_str::<Meta>(&text) else { continue };
            self.slots.lock().expect("slots lock").insert(meta.slot);
            if pid_alive(meta.pid) {
                // Firewall rules survive a controller restart; the proxy's
                // in-memory allowlist doesn't.
                if let Some((domains, _)) = &meta.allow {
                    self.egress_started.get_or_try_init(|| self.egress.start()).await?;
                    self.egress.register(slot_addrs(self.s.subnet, meta.slot).1, domains.clone());
                }
                let (tx, rx) = watch::channel(None);
                std::mem::forget(tx); // never resolves; stop() kills by pid
                self.vms.lock().expect("vms lock").insert(meta.id.clone(), Vm { meta, exit: rx, adopted: true });
            } else {
                self.cleanup(&meta).await;
            }
        }
        Ok(())
    }

    /// Creates the VM's file directory with the shared rootfs (hard link),
    /// a scratch disk copy and the config drive.
    async fn layout(&self, id: &str, scratch_src: &Path, config: &[u8]) -> anyhow::Result<Layout> {
        let rootfs = self.rootfs.get().context("rootfs not built")?;
        let vm_dir = self.vms_dir().join(id);
        tokio::fs::create_dir_all(&vm_dir).await?;
        let (files, kernel) = match &self.s.jailer {
            Some(_) => {
                let root = self.jail_dir(id).join("root");
                tokio::fs::create_dir_all(&root).await?;
                link_or_copy(Path::new(&self.s.kernel), &root.join("vmlinux")).await?;
                (root, "vmlinux".to_string())
            }
            None => (vm_dir.clone(), self.s.kernel.clone()),
        };
        link_or_copy(rootfs, &files.join("rootfs.ext4")).await?;
        run("cp", &["--sparse=always", scratch_src.to_str().unwrap(), files.join("scratch.ext4").to_str().unwrap()])
            .await?;
        tokio::fs::write(files.join("config.img"), config).await?;
        if let Some(j) = &self.s.jailer {
            let owner = format!("{}:{}", j.uid, j.gid);
            for f in ["scratch.ext4", "config.img"] {
                run("chown", &[&owner, files.join(f).to_str().unwrap()]).await?;
            }
            run("chown", &[&owner, files.to_str().unwrap()]).await?;
        }
        Ok(Layout { files, kernel, jail: self.s.jailer.clone() })
    }

    /// Spawns Firecracker for a layout inside the slot's netns.
    fn spawn(
        &self,
        id: &str,
        slot: u32,
        layout: &Layout,
        fc_args: &[&str],
        log: &Path,
    ) -> anyhow::Result<tokio::process::Child> {
        let log = std::fs::File::create(log)?;
        let ns = netns_name(slot);
        let mut cmd = match &layout.jail {
            Some(j) => {
                let mut c = Command::new(&j.bin);
                c.args([
                    "--id",
                    id,
                    "--exec-file",
                    &self.s.firecracker_bin,
                    "--uid",
                    &j.uid.to_string(),
                    "--gid",
                    &j.gid.to_string(),
                    "--chroot-base-dir",
                    self.s.state_dir.join("jail").to_str().unwrap(),
                    "--netns",
                    &format!("/var/run/netns/{ns}"),
                    "--",
                ]);
                c.args(fc_args);
                c
            }
            None => {
                let mut c = Command::new("ip");
                c.args(["netns", "exec", &ns, &self.s.firecracker_bin]);
                c.args(fc_args);
                c.current_dir(&layout.files);
                c
            }
        };
        cmd.stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
        cmd.spawn().context("spawning firecracker")
    }

    /// Boots a template VM until it waits for its config, then snapshots it.
    async fn build_template(&self, vcpus: u32, mem: u32) -> anyhow::Result<Template> {
        let rootfs = self.rootfs.get().context("rootfs not built")?;
        let key = content_hash(&format!(
            "{}|{}|{vcpus}|{mem}|{}|{}",
            rootfs.display(),
            self.s.kernel,
            self.s.firecracker_bin,
            self.s.jailer.is_some()
        ));
        let dir = self.s.state_dir.join(format!("snap-{key}"));
        if dir.join("ready").exists() {
            return Ok(Template { dir });
        }
        let t0 = Instant::now();
        let id = format!("template-{key}");
        let slot = self.alloc_slot()?;
        let res = async {
            let scratch = self.scratch.get().context("scratch not built")?.clone();
            let layout = self.layout(&id, &scratch, &vec![0u8; CONFIG_DRIVE_BYTES]).await?;
            self.setup_netns(slot, self.s.jailer.as_ref().map(|j| j.uid)).await?;
            let config = vm_config(&boot_args(&self.s.dns), vcpus, mem, &layout.kernel);
            tokio::fs::write(layout.files.join("vm.json"), serde_json::to_vec_pretty(&config)?).await?;
            let console = self.vms_dir().join(&id).join("console.log");
            let mut child =
                self.spawn(&id, slot, &layout, &["--api-sock", "vm.sock", "--config-file", "vm.json"], &console)?;
            let ready = async {
                let deadline = Instant::now() + Duration::from_secs(90);
                loop {
                    if tokio::fs::read_to_string(&console)
                        .await
                        .unwrap_or_default()
                        .contains("rgha-init: waiting for config")
                    {
                        return Ok(());
                    }
                    if Instant::now() > deadline {
                        bail!("template VM did not reach 'waiting for config' (see {})", console.display());
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
            .await;
            let snap = async {
                ready?;
                let sock = layout.files.join("vm.sock");
                fc_api(&sock, "PATCH", "/vm", &serde_json::json!({"state": "Paused"})).await?;
                fc_api(
                    &sock,
                    "PUT",
                    "/snapshot/create",
                    &serde_json::json!({"snapshot_type": "Full", "snapshot_path": "vmstate", "mem_file_path": "mem"}),
                )
                .await
            }
            .await;
            let _ = child.start_kill();
            let _ = child.wait().await;
            snap?;
            let staging = dir.with_extension("tmp");
            let _ = tokio::fs::remove_dir_all(&staging).await;
            tokio::fs::create_dir_all(&staging).await?;
            for f in ["vmstate", "mem", "scratch.ext4"] {
                tokio::fs::rename(layout.files.join(f), staging.join(f)).await?;
                run("chmod", &["0644", staging.join(f).to_str().unwrap()]).await?;
            }
            let rootfs_name = rootfs.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
            tokio::fs::write(staging.join("rootfs"), rootfs_name).await?;
            tokio::fs::write(staging.join("ready"), b"").await?;
            let _ = tokio::fs::remove_dir_all(&dir).await;
            tokio::fs::rename(&staging, &dir).await?;
            anyhow::Ok(())
        }
        .await;
        self.teardown(&id, slot).await;
        res?;
        tracing::info!(vcpus, mem, snapshot = %dir.display(), secs = format!("{:.1}", t0.elapsed().as_secs_f64()), "Firecracker snapshot template ready");
        Ok(Template { dir })
    }

    /// The snapshot template for a machine shape, built once (lazily).
    /// `None` if snapshots are off or building failed (cold boot instead).
    async fn template(&self, vcpus: u32, mem: u32) -> Option<Template> {
        if !self.s.snapshots {
            return None;
        }
        let mut templates = self.templates.lock().await;
        if let Some(t) = templates.get(&(vcpus, mem)) {
            return t.clone();
        }
        let t = match self.build_template(vcpus, mem).await {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::warn!(vcpus, mem, error = %format!("{e:#}"), "snapshot template failed; cold-booting VMs of this shape");
                None
            }
        };
        templates.insert((vcpus, mem), t.clone());
        t
    }

    async fn do_prepare(&self) -> anyhow::Result<()> {
        if run("id", &["-u"]).await? != "0" {
            bail!("the firecracker backend must run as root (namespaces, iptables, jailer)");
        }
        if !Path::new("/dev/kvm").exists() {
            bail!("/dev/kvm not found: the firecracker backend needs a KVM host");
        }
        if !Path::new(&self.s.kernel).exists() {
            bail!("guest kernel {} not found", self.s.kernel);
        }
        tokio::fs::create_dir_all(self.vms_dir()).await?;
        self.setup_network().await?;
        self.adopt_existing().await?;
        self.rootfs.get_or_try_init(|| self.build_rootfs()).await?;
        self.scratch.get_or_try_init(|| self.build_scratch()).await?;
        self.gc().await;
        Ok(())
    }

    /// Removes rootfs images and snapshot templates built for other
    /// configurations (they're large; a rootfs rebuild orphans them).
    async fn gc(&self) {
        let Some(current) = self.rootfs.get().and_then(|p| p.file_name()).and_then(|n| n.to_str()).map(str::to_string)
        else {
            return;
        };
        let Ok(mut dir) = tokio::fs::read_dir(&self.s.state_dir).await else { return };
        while let Ok(Some(e)) = dir.next_entry().await {
            let name = e.file_name().to_string_lossy().to_string();
            let stale = if name.starts_with("rootfs-") && name.ends_with(".ext4") {
                name != current
            } else if name.starts_with("snap-") {
                tokio::fs::read_to_string(e.path().join("rootfs")).await.map(|r| r != current).unwrap_or(true)
            } else {
                false
            };
            if stale {
                tracing::info!(path = %e.path().display(), "removing stale Firecracker artifact");
                let _ = tokio::fs::remove_dir_all(e.path()).await;
                let _ = tokio::fs::remove_file(e.path()).await;
            }
        }
    }

    async fn do_start(self: &Arc<Self>, spec: &RunnerSpec) -> anyhow::Result<String> {
        self.rootfs.get_or_try_init(|| self.build_rootfs()).await?;
        self.scratch.get_or_try_init(|| self.build_scratch()).await?;
        let vcpus = (spec.cpu_limit.ceil() as u32).clamp(1, 32);
        let mem = spec.memory_limit_mib.max(256);
        let template = self.template(vcpus, mem).await;

        let id = spec.name.clone();
        let slot = self.alloc_slot()?;
        let allow = match &spec.network {
            Network::Open => None,
            Network::Allowlist { domains, cidrs } => Some((domains.clone(), cidrs.clone())),
        };
        let res = async {
            if let Some((domains, cidrs)) = &allow {
                self.apply_egress(slot, domains, cidrs).await?;
            }
            self.start_vm(spec, &id, slot, vcpus, mem, template.as_ref(), allow.clone()).await
        }
        .await;
        if res.is_err() {
            if let Some((_, cidrs)) = &allow {
                self.remove_egress(slot, cidrs).await;
            }
            self.teardown(&id, slot).await;
        }
        res.map(|_| id)
    }

    async fn do_stop(&self, id: &str) -> anyhow::Result<()> {
        let pid = self.vms.lock().expect("vms lock").get(id).map(|v| v.meta.pid);
        if let Some(pid) = pid
            && pid_alive(pid)
        {
            run_ok("kill", &["-9", &pid.to_string()]).await;
        }
        // Orphans adopted from a previous process have no waiter task to
        // clean up after them; ours clean up when the process exits.
        let adopted = {
            let mut vms = self.vms.lock().expect("vms lock");
            if vms.get(id).is_some_and(|v| v.adopted) { vms.remove(id) } else { None }
        };
        if let Some(vm) = adopted {
            self.cleanup(&vm.meta).await;
        }
        Ok(())
    }

    async fn do_wait(&self, id: &str) -> anyhow::Result<Option<i32>> {
        let Some(mut rx) = self.vms.lock().expect("vms lock").get(id).map(|v| v.exit.clone()) else {
            return Ok(None);
        };
        loop {
            if let Some(code) = *rx.borrow() {
                return Ok(Some(code));
            }
            if rx.changed().await.is_err() {
                return Ok(None);
            }
        }
    }

    async fn do_list(&self, class: &str) -> anyhow::Result<Vec<Instance>> {
        self.sweep_dead_adopted().await;
        Ok(self
            .vms
            .lock()
            .expect("vms lock")
            .values()
            .filter(|v| v.meta.class == class && pid_alive(v.meta.pid))
            .map(|v| Instance { id: v.meta.id.clone(), runner_name: v.meta.runner_name.clone() })
            .collect())
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_vm(
        self: &Arc<Self>,
        spec: &RunnerSpec,
        id: &str,
        slot: u32,
        vcpus: u32,
        mem: u32,
        template: Option<&Template>,
        allow: Option<(Vec<String>, Vec<String>)>,
    ) -> anyhow::Result<()> {
        let epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs_f64();
        let config = config_drive(&spec.jit_config, epoch)?;
        self.setup_netns(slot, self.s.jailer.as_ref().map(|j| j.uid)).await?;
        let console = self.vms_dir().join(id).join("console.log");

        let mut restored = None;
        if let Some(t) = template {
            let layout = self.layout(id, &t.dir.join("scratch.ext4"), &config).await?;
            link_or_copy(&t.dir.join("vmstate"), &layout.files.join("vmstate")).await?;
            link_or_copy(&t.dir.join("mem"), &layout.files.join("mem")).await?;
            let mut child = self.spawn(id, slot, &layout, &["--api-sock", "vm.sock"], &console)?;
            let load = fc_api(
                &layout.files.join("vm.sock"),
                "PUT",
                "/snapshot/load",
                &serde_json::json!({
                    "snapshot_path": "vmstate",
                    "mem_backend": {"backend_type": "File", "backend_path": "mem"},
                    "resume_vm": true,
                    "clock_realtime": true,
                }),
            )
            .await;
            match load {
                Ok(()) => restored = Some(child),
                Err(e) => {
                    tracing::warn!(vm = id, error = %format!("{e:#}"), "snapshot restore failed; cold-booting");
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    let _ = tokio::fs::remove_dir_all(self.vms_dir().join(id)).await;
                    let _ = tokio::fs::remove_dir_all(self.jail_dir(id)).await;
                }
            }
        }
        let child = match restored {
            Some(c) => c,
            None => {
                let scratch = self.scratch.get().context("scratch not built")?.clone();
                let layout = self.layout(id, &scratch, &config).await?;
                let cfg = vm_config(&boot_args(&self.s.dns), vcpus, mem, &layout.kernel);
                tokio::fs::write(layout.files.join("vm.json"), serde_json::to_vec_pretty(&cfg)?).await?;
                self.spawn(id, slot, &layout, &["--no-api", "--config-file", "vm.json"], &console)?
            }
        };
        self.track(spec, id, slot, child, allow).await
    }

    async fn track(
        self: &Arc<Self>,
        spec: &RunnerSpec,
        id: &str,
        slot: u32,
        mut child: tokio::process::Child,
        allow: Option<(Vec<String>, Vec<String>)>,
    ) -> anyhow::Result<()> {
        let pid = child.id().context("firecracker pid")?;
        let meta = Meta {
            id: id.to_string(),
            class: spec.class.clone(),
            runner_name: spec.name.clone(),
            slot,
            pid,
            jailed: self.s.jailer.is_some(),
            allow,
        };
        tokio::fs::write(self.vms_dir().join(id).join("meta.json"), serde_json::to_vec(&meta)?).await?;
        let cleanup_meta: Meta = serde_json::from_slice(&serde_json::to_vec(&meta)?)?;
        let (tx, rx) = watch::channel(None);
        self.vms.lock().expect("vms lock").insert(id.to_string(), Vm { meta, exit: rx, adopted: false });

        // Waiter: VM exit = runner done. Keep the console log, then clean up.
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            let code = child.wait().await.ok().and_then(|s| s.code()).unwrap_or(-1);
            let logs = inner.s.state_dir.join("logs");
            let _ = tokio::fs::create_dir_all(&logs).await;
            let console = inner.vms_dir().join(&cleanup_meta.id).join("console.log");
            let _ = tokio::fs::copy(console, logs.join(format!("{}.log", cleanup_meta.id))).await;
            inner.cleanup(&cleanup_meta).await;
            let _ = tx.send(Some(code));
            // Receivers already handed out keep the exit code.
            inner.vms.lock().expect("vms lock").remove(&cleanup_meta.id);
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_get_disjoint_veth_slash30s() {
        assert_eq!(slot_addrs([10, 213], 0), (Ipv4Addr::new(10, 213, 0, 1), Ipv4Addr::new(10, 213, 0, 2)));
        assert_eq!(slot_addrs([10, 213], 1), (Ipv4Addr::new(10, 213, 0, 5), Ipv4Addr::new(10, 213, 0, 6)));
        assert_eq!(slot_addrs([10, 213], 64), (Ipv4Addr::new(10, 213, 1, 1), Ipv4Addr::new(10, 213, 1, 2)));
        assert_eq!(slot_addrs([10, 213], MAX_SLOTS - 1).1, Ipv4Addr::new(10, 213, 255, 254));
        assert!(veth_name(MAX_SLOTS - 1).len() <= 15, "IFNAMSIZ");
        assert!(veth_name(7).starts_with("rgha"), "matched by the rgha+ firewall rules");
    }

    #[test]
    fn config_drive_is_fixed_size_and_jit_never_on_cmdline() {
        let args = boot_args("1.1.1.1");
        assert!(args.contains("root=/dev/vda ro") && args.contains("init=/sbin/rgha-init"));
        assert!(args.contains(&format!("ip={GUEST_IP}::{TAP_HOST_IP}:255.255.255.252::eth0:off")));
        let d = config_drive("SECRET", 1700000000.5).unwrap();
        assert_eq!(d.len(), CONFIG_DRIVE_BYTES, "same size as the snapshot's drive");
        let text = String::from_utf8_lossy(&d);
        assert!(text.starts_with("rgha1 1700000000.500\nSECRET\n"));
        assert!(config_drive(&"x".repeat(CONFIG_DRIVE_BYTES), 0.0).is_err());
    }

    #[test]
    fn vm_config_uses_relative_paths_for_snapshot_portability() {
        let v = vm_config("args", 2, 2048, "vmlinux");
        let drives = v["drives"].as_array().unwrap();
        assert_eq!(drives[0]["path_on_host"], "rootfs.ext4");
        assert_eq!(drives[0]["is_read_only"], true, "rootfs is shared read-only");
        assert_eq!(drives[2]["path_on_host"], "scratch.ext4");
        assert_eq!(v["network-interfaces"][0]["host_dev_name"], "tap0");
        assert!(v.get("entropy").is_some(), "virtio-rng for clones");
    }

    #[test]
    fn egress_rules_redirect_web_allow_dns_and_drop_the_rest() {
        let r = egress_rules(5, "1.1.1.1", &["203.0.113.0/24".into()]);
        let flat: Vec<String> = r.iter().map(|(t, c, a)| format!("{t} {c} {}", a.join(" "))).collect();
        assert!(flat.contains(&"nat RGHA-PRE -i rgha-v5 -p tcp --dport 443 -j REDIRECT --to-ports 15443".to_string()));
        assert!(flat.contains(&"nat RGHA-PRE -i rgha-v5 -p tcp --dport 80 -j REDIRECT --to-ports 15080".to_string()));
        // Each filter rule is inserted at the top, so the DROP (first) ends up last.
        let filter: Vec<&String> = flat.iter().filter(|f| f.starts_with("filter")).collect();
        assert_eq!(filter.first().unwrap().as_str(), "filter RGHA-FWD -i rgha-v5 -j DROP");
        assert!(filter.iter().any(|f| f.ends_with("-d 203.0.113.0/24 -j ACCEPT")));
        assert!(filter.iter().any(|f| f.contains("-p udp -d 1.1.1.1 --dport 53 -j ACCEPT")));
    }

    #[test]
    fn dockerfile_layers_preload_then_init() {
        let s = FirecrackerSettings {
            image: "img:1".into(),
            image_commands: vec!["RUN echo hi".into()],
            preload: Preload { node: vec!["22".into()], ..Default::default() },
            firecracker_bin: "firecracker".into(),
            jailer: None,
            kernel: "/k".into(),
            state_dir: "/s".into(),
            docker_bin: "docker".into(),
            rootfs_size_gib: 8,
            scratch_size_gib: 16,
            snapshots: true,
            subnet: [10, 213],
            uplink: None,
            dns: "1.1.1.1".into(),
        };
        let d = dockerfile(&s);
        assert!(d.starts_with("FROM img:1\n"));
        let (node, user, init) =
            (d.find("node").unwrap(), d.find("RUN echo hi").unwrap(), d.find("COPY rgha-init").unwrap());
        assert!(node < user && user < init);
        assert!(d.contains("/etc/rgha/image.env"), "image ENV recorded for the guest");
        assert!(d.contains("/mnt/scratch /mnt/newroot"), "overlay mount points exist in the read-only rootfs");
        assert_ne!(content_hash("a"), content_hash("b"));
    }
}
