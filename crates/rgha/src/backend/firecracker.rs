//! Firecracker backend: one microVM per job on a KVM host.
//!
//! - **Rootfs**: built once from the runner image (+ `preload` layers + the
//!   guest init) with Docker, converted to ext4, cached by content hash.
//!   Each VM boots a copy of it.
//! - **Config**: the single-use JIT config is written to a small read-only
//!   raw drive (`/dev/vdb`), not the kernel command line.
//! - **Network**: a tap device and a /30 per VM, NAT out of the uplink.
//!   Guests can't reach the host, private ranges, or link-local/metadata
//!   addresses (`RGHA-FWD` chain, inserted into `DOCKER-USER` when Docker
//!   is present so it applies before Docker's own rules).
//! - **Isolation**: KVM; optionally the Firecracker jailer (chroot,
//!   unprivileged uid, its own /dev/kvm).
//! - **Lifecycle**: the guest powers off when the runner exits; Firecracker
//!   exits; rgha tears down the tap and disk. VM state lives under
//!   `state_dir/vms` so a restarted controller can reconcile.
//!
//! Requires root (tap devices, iptables, jailer) and `/dev/kvm`.

use std::collections::{BTreeSet, HashMap};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::sync::{OnceCell, watch};

use super::{Backend, Instance, Network, RunnerSpec};
use crate::image::Preload;

pub const GUEST_INIT: &str = include_str!("../../assets/firecracker/rgha-init");
const MAX_SLOTS: u32 = 16_384; // /30 per VM inside a /16

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
    /// First two octets of the VM /16, e.g. `[10, 213]`.
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
}

struct Vm {
    meta: Meta,
    exit: watch::Receiver<Option<i32>>,
}

pub struct FirecrackerBackend(Arc<Inner>);

struct Inner {
    s: FirecrackerSettings,
    rootfs: OnceCell<PathBuf>,
    uplink: OnceCell<String>,
    vms: Mutex<HashMap<String, Vm>>,
    slots: Mutex<BTreeSet<u32>>,
}

// ------------------------------------------------------------- pure helpers

/// Host and guest addresses of a slot's /30.
pub(crate) fn slot_addrs(subnet: [u8; 2], slot: u32) -> (Ipv4Addr, Ipv4Addr) {
    let base = u32::from(Ipv4Addr::new(subnet[0], subnet[1], 0, 0)) + slot * 4;
    (Ipv4Addr::from(base + 1), Ipv4Addr::from(base + 2))
}

pub(crate) fn tap_name(slot: u32) -> String {
    format!("rgha{slot}")
}

pub(crate) fn boot_args(host: Ipv4Addr, guest: Ipv4Addr, dns: &str) -> String {
    format!(
        "console=ttyS0 reboot=k panic=1 pci=off quiet root=/dev/vda rw init=/sbin/rgha-init \
         ip={guest}::{host}:255.255.255.252::eth0:off rgha.dns={dns}"
    )
}

/// The JIT config padded with NULs to a whole number of 512-byte sectors.
pub(crate) fn config_drive(jit: &str) -> Vec<u8> {
    let mut b = jit.as_bytes().to_vec();
    let len = b.len().div_ceil(512).max(1) * 512;
    b.resize(len, 0);
    b
}

pub(crate) fn guest_mac(slot: u32) -> String {
    format!("06:00:{:02x}:{:02x}:{:02x}:02", (slot >> 16) & 0xff, (slot >> 8) & 0xff, slot & 0xff)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn vm_config(
    kernel: &str,
    rootfs: &str,
    config: &str,
    boot_args: &str,
    vcpus: u32,
    mem_mib: u32,
    tap: &str,
    mac: &str,
) -> serde_json::Value {
    serde_json::json!({
        "boot-source": { "kernel_image_path": kernel, "boot_args": boot_args },
        "drives": [
            { "drive_id": "rootfs", "path_on_host": rootfs, "is_root_device": true, "is_read_only": false },
            { "drive_id": "config", "path_on_host": config, "is_root_device": false, "is_read_only": true },
        ],
        "machine-config": { "vcpu_count": vcpus, "mem_size_mib": mem_mib, "smt": false },
        "network-interfaces": [ { "iface_id": "eth0", "guest_mac": mac, "host_dev_name": tap } ],
    })
}

/// Dockerfile for the rootfs: runner image + preloads + user commands + init.
pub(crate) fn dockerfile(s: &FirecrackerSettings) -> String {
    let mut lines = vec![format!("FROM {}", s.image)];
    lines.extend(s.preload.dockerfile_commands());
    lines.extend(s.image_commands.iter().cloned());
    lines.push("USER root".into());
    lines.push("COPY rgha-init /sbin/rgha-init".into());
    lines.push("RUN chmod 0755 /sbin/rgha-init".into());
    // Docker ENV doesn't survive `docker export`; record it for the guest init.
    lines.push(
        "RUN mkdir -p /etc/rgha && env | grep -Ev '^(HOSTNAME|HOME|PWD|SHLVL|_|OLDPWD)=' > /etc/rgha/image.env".into(),
    );
    lines.join("\n") + "\n"
}

fn content_hash(s: &str) -> String {
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

/// Like `run` but ignores "already exists"/"no such" style failures.
async fn run_ok(cmd: &str, args: &[&str]) {
    let _ = Command::new(cmd).args(args).stdout(Stdio::null()).stderr(Stdio::null()).status().await;
}

fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

impl FirecrackerBackend {
    pub fn new(s: FirecrackerSettings) -> Self {
        Self(Arc::new(Inner {
            s,
            rootfs: OnceCell::new(),
            uplink: OnceCell::new(),
            vms: Mutex::new(HashMap::new()),
            slots: Mutex::new(BTreeSet::new()),
        }))
    }
}

impl Inner {
    fn vms_dir(&self) -> PathBuf {
        self.s.state_dir.join("vms")
    }

    fn jail_root(&self, id: &str) -> PathBuf {
        self.s.state_dir.join("jail").join("firecracker").join(id).join("root")
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
        let fwd = |args: &[&str]| {
            let mut v = vec!["-A", "RGHA-FWD"];
            v.extend_from_slice(args);
            v.into_iter().map(str::to_string).collect::<Vec<_>>()
        };
        let mut rules =
            vec![fwd(&["-o", "rgha+", "-m", "conntrack", "--ctstate", "ESTABLISHED,RELATED", "-j", "ACCEPT"])];
        for net in ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "169.254.0.0/16", "100.64.0.0/10"] {
            rules.push(fwd(&["-i", "rgha+", "-d", net, "-j", "DROP"]));
        }
        rules.push(fwd(&["-i", "rgha+", "-o", &up, "-j", "ACCEPT"]));
        rules.push(fwd(&["-i", "rgha+", "-j", "DROP"]));
        for r in rules {
            let r: Vec<&str> = r.iter().map(String::as_str).collect();
            run("iptables", &r).await?;
        }
        // Docker's FORWARD policy is DROP; its DOCKER-USER chain runs first.
        let hook = if run("iptables", &["-L", "DOCKER-USER", "-n"]).await.is_ok() { "DOCKER-USER" } else { "FORWARD" };
        if run("iptables", &["-C", hook, "-j", "RGHA-FWD"]).await.is_err() {
            run("iptables", &["-I", hook, "-j", "RGHA-FWD"]).await?;
        }
        // Guests can't talk to the host itself.
        if run("iptables", &["-C", "INPUT", "-i", "rgha+", "-j", "DROP"]).await.is_err() {
            run("iptables", &["-I", "INPUT", "-i", "rgha+", "-j", "DROP"]).await?;
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
        let res = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
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
        tokio::fs::rename(self.s.state_dir.join(format!("rootfs-{hash}.ext4.tmp")), &out).await?;
        let _ = tokio::fs::remove_dir_all(&build).await;
        tracing::info!(rootfs = %out.display(), "Firecracker rootfs ready");
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

    async fn cleanup(&self, meta: &Meta) {
        run_ok("ip", &["link", "del", &tap_name(meta.slot)]).await;
        let _ = tokio::fs::remove_dir_all(self.vms_dir().join(&meta.id)).await;
        if meta.jailed {
            let _ = tokio::fs::remove_dir_all(self.s.state_dir.join("jail").join("firecracker").join(&meta.id)).await;
        }
        self.free_slot(meta.slot);
    }

    /// Adopts VMs left by a previous controller process: live ones are
    /// tracked (so reconcile can decide), dead ones are cleaned up.
    async fn adopt_existing(&self) -> anyhow::Result<()> {
        let mut dir = match tokio::fs::read_dir(self.vms_dir()).await {
            Ok(d) => d,
            Err(_) => return Ok(()),
        };
        while let Some(e) = dir.next_entry().await? {
            let Ok(text) = tokio::fs::read_to_string(e.path().join("meta.json")).await else { continue };
            let Ok(meta) = serde_json::from_str::<Meta>(&text) else { continue };
            self.slots.lock().expect("slots lock").insert(meta.slot);
            if pid_alive(meta.pid) {
                let (tx, rx) = watch::channel(None);
                std::mem::forget(tx); // never resolves; stop() kills by pid
                self.vms.lock().expect("vms lock").insert(meta.id.clone(), Vm { meta, exit: rx });
            } else {
                self.cleanup(&meta).await;
            }
        }
        Ok(())
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

impl Inner {
    async fn do_prepare(&self) -> anyhow::Result<()> {
        if run("id", &["-u"]).await? != "0" {
            bail!("the firecracker backend must run as root (tap devices, iptables, jailer)");
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
        Ok(())
    }

    async fn do_start(self: &Arc<Self>, spec: &RunnerSpec) -> anyhow::Result<String> {
        if spec.network != Network::Open {
            bail!("firecracker backend: egress allowlists are not implemented yet (strawgate/rgha#22)");
        }
        let rootfs = self.rootfs.get_or_try_init(|| self.build_rootfs()).await?.clone();
        let id = spec.name.clone();
        let slot = self.alloc_slot()?;
        let res = self.start_inner(spec, &id, slot, &rootfs).await;
        if res.is_err() {
            run_ok("ip", &["link", "del", &tap_name(slot)]).await;
            let _ = tokio::fs::remove_dir_all(self.vms_dir().join(&id)).await;
            let _ = tokio::fs::remove_dir_all(self.s.state_dir.join("jail").join("firecracker").join(&id)).await;
            self.free_slot(slot);
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
        // Orphans adopted from a previous process have no waiter task.
        let adopted = self.vms.lock().expect("vms lock").remove(id).filter(|v| v.exit.borrow().is_none());
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
        Ok(self
            .vms
            .lock()
            .expect("vms lock")
            .values()
            .filter(|v| v.meta.class == class && pid_alive(v.meta.pid))
            .map(|v| Instance { id: v.meta.id.clone(), runner_name: v.meta.runner_name.clone() })
            .collect())
    }
}

impl Inner {
    async fn start_inner(
        self: &Arc<Self>,
        spec: &RunnerSpec,
        id: &str,
        slot: u32,
        rootfs: &Path,
    ) -> anyhow::Result<()> {
        let (host, guest) = slot_addrs(self.s.subnet, slot);
        let tap = tap_name(slot);
        let vm_dir = self.vms_dir().join(id);
        tokio::fs::create_dir_all(&vm_dir).await?;

        // Files live in the jail root when jailed (paths relative to it).
        let (files_dir, kernel_path, owner) = match &self.s.jailer {
            Some(j) => {
                let root = self.jail_root(id);
                tokio::fs::create_dir_all(&root).await?;
                // Hard link when possible (same filesystem), else copy.
                let k = root.join("vmlinux");
                if tokio::fs::hard_link(&self.s.kernel, &k).await.is_err() {
                    tokio::fs::copy(&self.s.kernel, &k).await?;
                }
                (root, "vmlinux".to_string(), Some((j.uid, j.gid)))
            }
            None => (vm_dir.clone(), self.s.kernel.clone(), None),
        };

        run("ip", &["tuntap", "add", &tap, "mode", "tap"]).await?;
        if let Some((uid, _)) = owner {
            run("ip", &["tuntap", "del", &tap, "mode", "tap"]).await?;
            run("ip", &["tuntap", "add", &tap, "mode", "tap", "user", &uid.to_string()]).await?;
        }
        run("ip", &["addr", "add", &format!("{host}/30"), "dev", &tap]).await?;
        run("ip", &["link", "set", &tap, "up"]).await?;

        let rootfs_copy = files_dir.join("rootfs.ext4");
        run("cp", &["--sparse=always", rootfs.to_str().unwrap(), rootfs_copy.to_str().unwrap()]).await?;
        tokio::fs::write(files_dir.join("config.img"), config_drive(&spec.jit_config)).await?;

        let (rootfs_ref, config_ref) = if owner.is_some() {
            ("rootfs.ext4".to_string(), "config.img".to_string())
        } else {
            (rootfs_copy.display().to_string(), files_dir.join("config.img").display().to_string())
        };
        let vcpus = (spec.cpu_limit.ceil() as u32).clamp(1, 32);
        let mem = spec.memory_limit_mib.max(256);
        let config = vm_config(
            &kernel_path,
            &rootfs_ref,
            &config_ref,
            &boot_args(host, guest, &self.s.dns),
            vcpus,
            mem,
            &tap,
            &guest_mac(slot),
        );
        tokio::fs::write(files_dir.join("vm.json"), serde_json::to_vec_pretty(&config)?).await?;
        if let Some((uid, gid)) = owner {
            run("chown", &["-R", &format!("{uid}:{gid}"), files_dir.to_str().unwrap()]).await?;
        }

        let log = std::fs::File::create(vm_dir.join("console.log"))?;
        let mut cmd = match &self.s.jailer {
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
                    "--",
                    "--no-api",
                    "--config-file",
                    "vm.json",
                ]);
                c
            }
            None => {
                let mut c = Command::new(&self.s.firecracker_bin);
                c.args(["--no-api", "--config-file", files_dir.join("vm.json").to_str().unwrap()]);
                c
            }
        };
        cmd.stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
        let mut child = cmd.spawn().context("spawning firecracker")?;
        let pid = child.id().context("firecracker pid")?;
        let meta = Meta {
            id: id.to_string(),
            class: spec.class.clone(),
            runner_name: spec.name.clone(),
            slot,
            pid,
            jailed: owner.is_some(),
        };
        tokio::fs::write(vm_dir.join("meta.json"), serde_json::to_vec(&meta)?).await?;

        let (tx, rx) = watch::channel(None);
        let cleanup_meta: Meta = serde_json::from_slice(&serde_json::to_vec(&meta)?)?;
        self.vms.lock().expect("vms lock").insert(id.to_string(), Vm { meta, exit: rx });

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
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_get_disjoint_slash30s() {
        assert_eq!(slot_addrs([10, 213], 0), (Ipv4Addr::new(10, 213, 0, 1), Ipv4Addr::new(10, 213, 0, 2)));
        assert_eq!(slot_addrs([10, 213], 1), (Ipv4Addr::new(10, 213, 0, 5), Ipv4Addr::new(10, 213, 0, 6)));
        assert_eq!(slot_addrs([10, 213], 64), (Ipv4Addr::new(10, 213, 1, 1), Ipv4Addr::new(10, 213, 1, 2)));
        assert_eq!(slot_addrs([10, 213], MAX_SLOTS - 1).1, Ipv4Addr::new(10, 213, 255, 254));
        assert!(tap_name(MAX_SLOTS - 1).len() <= 15, "IFNAMSIZ");
    }

    #[test]
    fn jit_never_on_kernel_cmdline_and_drive_is_sector_aligned() {
        let args = boot_args(Ipv4Addr::new(10, 213, 0, 1), Ipv4Addr::new(10, 213, 0, 2), "1.1.1.1");
        assert!(args.contains("ip=10.213.0.2::10.213.0.1:255.255.255.252::eth0:off"));
        assert!(args.contains("init=/sbin/rgha-init") && args.contains("rgha.dns=1.1.1.1"));
        let d = config_drive("SECRET");
        assert_eq!(d.len(), 512);
        assert!(d.starts_with(b"SECRET") && d[6..].iter().all(|b| *b == 0));
        assert_eq!(config_drive(&"x".repeat(513)).len(), 1024);
    }

    #[test]
    fn vm_config_shape() {
        let v = vm_config("k", "r", "c", "args", 2, 2048, "rgha3", &guest_mac(3));
        assert_eq!(v["drives"][1]["is_read_only"], true);
        assert_eq!(v["machine-config"]["vcpu_count"], 2);
        assert_eq!(v["network-interfaces"][0]["host_dev_name"], "rgha3");
        assert_eq!(guest_mac(3), "06:00:00:00:03:02");
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
        assert_ne!(content_hash("a"), content_hash("b"));
    }
}
