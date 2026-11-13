use std::{future::Future, time::Duration};

use anyhow::{bail, ensure, Context, Result};
use fube_node::control::wire::{self, Link};
use tokio::{net::TcpListener, time::Instant};

use crate::{Config, Phase, State, Status};

fn publish(
    state: &State,
    previous: &mut Option<Status>,
    observe: &mut impl FnMut(&Status) -> Result<()>,
) -> Result<()> {
    let status = state.status();
    if previous.as_ref() != Some(&status) {
        observe(&status)?;
        *previous = Some(status);
    }
    Ok(())
}

/// Serve the original three connections until requested native deletion and
/// subsequent node disconnects. The caller, not the FM, owns the node processes.
pub async fn run(
    config: Config,
    listener: TcpListener,
    shutdown: impl Future<Output = Result<()>>,
    observe: impl FnMut(&Status) -> Result<()>,
) -> Result<()> {
    run_guarded(config, listener, shutdown, std::future::pending(), observe).await
}

/// Invalidation stops new FM intent and withdraws publication. It does not
/// certify that a remote application writer has stopped.
pub async fn run_guarded(
    config: Config,
    listener: TcpListener,
    shutdown: impl Future<Output = Result<()>>,
    invalidated: impl Future<Output = String>,
    mut observe: impl FnMut(&Status) -> Result<()>,
) -> Result<()> {
    config.validate()?;
    ensure!(
        listener.local_addr()? == config.fm.address.into(),
        "FM listener does not match the configured concrete address"
    );
    let mut state = State::new(config.clone())?;
    let mut links = vec![None; 3];
    let mut previous = None;
    let result = {
        tokio::pin!(invalidated);
        tokio::select! {
            biased;
            reason = &mut invalidated => Err(anyhow::anyhow!("FM authority invalidated: {reason}")),
            result = drive(
                &config,
                listener,
                shutdown,
                &mut state,
                &mut links,
                &mut previous,
                &mut observe,
            ) => result,
        }
    };
    let mut failures = Vec::new();
    if let Err(error) = result {
        state.fail(format!("{error:#}"));
        failures.push(format!("{error:#}"));
        if let Err(error) = publish(&state, &mut previous, &mut observe) {
            failures.push(format!("reporting control uncertainty: {error:#}"));
        }
    }
    for (node, link) in links.into_iter().enumerate() {
        if let Some(link) = link {
            if let Err(error) = link.close().await {
                failures.push(format!("closing original node {node} channel: {error}"));
            }
        }
    }
    if !failures.is_empty() {
        bail!("{}", failures.join("; "));
    }
    Ok(())
}

async fn drive(
    config: &Config,
    listener: TcpListener,
    shutdown: impl Future<Output = Result<()>>,
    state: &mut State,
    links: &mut [Option<Link>],
    previous: &mut Option<Status>,
    observe: &mut impl FnMut(&Status) -> Result<()>,
) -> Result<()> {
    let started = Instant::now();
    let mut stopping = false;
    let mut admitted = [false; 3];
    tokio::pin!(shutdown);
    loop {
        for outbound in state.reconcile(started.elapsed())? {
            links[outbound.node]
                .as_ref()
                .context("native command has no original node channel")?
                .send(outbound.message)
                .await
                .map_err(anyhow::Error::msg)?;
        }
        publish(state, previous, observe)?;
        if state.phase() == Phase::Stopped {
            return Ok(());
        }
        tokio::select! {
            result = &mut shutdown, if !stopping => {
                result?;
                state.request_shutdown(started.elapsed())?;
                stopping = true;
            }
            accepted = listener.accept() => {
                let (stream, source) = accepted?;
                let node = config.nodes.iter().position(|node| source.ip() == *node.address.ip());
                if let Some(node) = node.filter(|node| !admitted[*node] && !stopping) {
                    let channel = fube_transport::accept_unsecured_tcp(
                        stream,
                        wire::composition(true)?,
                        wire::limits(),
                        wire::deadline(),
                    ).await?;
                    links[node] = Some(Link::new(
                        channel,
                        config.fm.identity.native().map_err(anyhow::Error::msg)?,
                        config.nodes[node].identity.native().map_err(anyhow::Error::msg)?,
                        config.generation()?,
                    ).map_err(anyhow::Error::msg)?);
                    admitted[node] = true;
                } else {
                    eprintln!("{}", serde_json::json!({
                        "event": "connection_rejected",
                        "source": source.to_string(),
                        "reason": "unconfigured, duplicate or shutdown connection; reconnect is unsupported"
                    }));
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(5)) => {}
        }
        for (node, slot) in links.iter_mut().enumerate() {
            let Some(link) = slot.clone() else {
                continue;
            };
            let mut incoming = match link
                .receive(Instant::now() + Duration::from_millis(1))
                .await
            {
                Ok(Some(incoming)) => incoming,
                Ok(None) => continue,
                Err(error) => {
                    state
                        .disconnected(node)
                        .with_context(|| format!("original node {node} receive failed: {error}"))?;
                    link.close().await.map_err(anyhow::Error::msg)?;
                    *slot = None;
                    continue;
                }
            };
            let response = state.observe(node, &incoming.message)?;
            if let Some(response) = response {
                if let Some(reply) = incoming.reply.take() {
                    reply
                        .reply(
                            link.seal(response).map_err(anyhow::Error::msg)?,
                            wire::deadline(),
                        )
                        .await?;
                } else {
                    ensure!(
                        response.properties()?.action != "NodeUpAck",
                        "NodeUp is missing its original correlated reply context"
                    );
                    link.send(response).await.map_err(anyhow::Error::msg)?;
                }
            } else {
                ensure!(
                    incoming.reply.is_none(),
                    "unexpected correlated native request"
                );
            }
            if let Some(handoff) = incoming.handoff.as_mut() {
                handoff.acknowledge();
            }
        }
        publish(state, previous, observe)?;
    }
}
