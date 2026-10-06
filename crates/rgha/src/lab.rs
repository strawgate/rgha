//! Experiments that inform design decisions. Not part of normal operation.
//!
//! `hibernate-prepare` / `restore`: can a registered, listening runner be
//! memory-snapshotted and restored on demand, making warm pools cost nothing
//! while idle? Run with the controller for that class stopped, so the
//! restored runner is the only one that can take the next job.

use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use rgha_modal::{Client as Modal, Profile};

use crate::backend::{Network, RunnerSpec};
use crate::config::{BackendConfig, Config};

pub async fn modal_for(cfg: &Config, backend: &str) -> anyhow::Result<(Modal, String, String, BackendConfig)> {
    let b = cfg.backends.get(backend).with_context(|| format!("unknown backend {backend}"))?.clone();
    let BackendConfig::Modal { app, image, image_commands, profile, preload, docker, .. } = &b else {
        bail!("lab experiments need a modal backend");
    };
    let modal = Modal::connect(Profile::load(profile.as_deref())?).await?;
    let app_id = modal.app_get_or_create(app).await?;
    let mut cmds = preload.dockerfile_commands();
    cmds.extend(image_commands.iter().cloned());
    if *docker {
        cmds.extend(crate::backend::modal_docker_commands());
    }
    let image_id = modal.image_from_registry(&app_id, image, &cmds).await?;
    Ok((modal, app_id, image_id, b))
}

pub async fn hibernate_prepare(cfg: &Config, gh: &rgha_scaleset::Client, class_name: &str) -> anyhow::Result<()> {
    let class = cfg.classes.iter().find(|c| c.name == class_name).context("unknown class")?;
    let (modal, app_id, image_id, b) = modal_for(cfg, &class.backend).await?;
    let BackendConfig::Modal { runtime, regions, docker, .. } = &b else { unreachable!() };

    let group = gh.get_runner_group_by_name(&cfg.github.runner_group).await?;
    let ss = match gh.get_scale_set(group.id, &class.name).await? {
        Some(ss) => ss,
        None => {
            gh.create_scale_set(rgha_scaleset::RunnerScaleSet {
                name: class.name.clone(),
                runner_group_id: group.id,
                labels: vec![rgha_scaleset::Label::system(class.name.clone())],
                runner_setting: rgha_scaleset::RunnerSetting { disable_update: true },
                ..Default::default()
            })
            .await?
        }
    };
    let name = format!("{}-hib-{}", class.name, &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let jit = gh.generate_jit_config(ss.id, &name, "_work").await?;
    let spec = RunnerSpec {
        name: name.clone(),
        class: class.name.clone(),
        jit_config: jit.encoded_jit_config,
        cpu: class.cpu,
        cpu_limit: class.cpu_cap(),
        memory_mib: class.memory_mib,
        memory_limit_mib: class.memory_cap_mib(),
        timeout: Duration::from_secs(3600),
        network: Network::for_class(class),
        job_started_hook: None,
    };
    let mut sb = crate::backend::modal_sandbox_spec(&spec, &image_id, runtime.clone(), regions.clone(), *docker);
    sb.enable_snapshot = true;

    let t0 = Instant::now();
    let id = modal.sandbox_create(&app_id, &sb).await?;
    // Wait until GitHub sees the runner online (session established).
    let online = loop {
        if let Some(r) = gh.get_runner_by_name(&name).await?
            && r.is_online()
        {
            break t0.elapsed();
        }
        if t0.elapsed() > Duration::from_secs(120) {
            bail!("runner {name} never came online");
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    };
    tokio::time::sleep(Duration::from_secs(3)).await;
    let t1 = Instant::now();
    let snapshot = modal.sandbox_snapshot(&id).await?;
    let snap = t1.elapsed();
    modal.sandbox_terminate(&id).await?;
    println!(
        "{}",
        serde_json::json!({
            "runner": name, "runner_id": jit.runner.id, "snapshot_id": snapshot,
            "create_to_online_secs": online.as_secs_f64(), "snapshot_secs": snap.as_secs_f64(),
        })
    );
    Ok(())
}

pub async fn restore(cfg: &Config, backend: &str, snapshot_id: &str) -> anyhow::Result<()> {
    let (modal, ..) = modal_for(cfg, backend).await?;
    let t0 = Instant::now();
    let id = modal.sandbox_restore(snapshot_id).await?;
    println!("{}", serde_json::json!({ "sandbox_id": id, "restore_to_running_secs": t0.elapsed().as_secs_f64() }));
    Ok(())
}

pub async fn cleanup(
    cfg: &Config,
    gh: &rgha_scaleset::Client,
    backend: &str,
    class: &str,
    sandbox_id: Option<&str>,
) -> anyhow::Result<()> {
    if let Some(id) = sandbox_id {
        let (modal, ..) = modal_for(cfg, backend).await?;
        modal.sandbox_terminate(id).await?;
        println!("terminated {id}");
    }
    let group = gh.get_runner_group_by_name(&cfg.github.runner_group).await?;
    if let Some(ss) = gh.get_scale_set(group.id, class).await? {
        gh.delete_scale_set(ss.id).await?;
        println!("deleted scale set {class} ({})", ss.id);
    }
    Ok(())
}
