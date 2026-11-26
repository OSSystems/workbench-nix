//! workbench runs persistent per-person workspaces: one long-lived
//! cloud-hypervisor VM each, with the web app its owner opens, declared in
//! Nix. Every workspace is its own systemd unit whose main process this
//! binary becomes (`run`); the memory daemon and the ops commands only talk
//! to running VMs through their API and vsock sockets, so restarting them
//! never stops a VM.
#![cfg(target_os = "linux")]

pub mod agent;
pub mod cli;
pub mod config;
pub mod fleet;
pub mod launch;
pub mod memoryd;
pub mod self_service;
pub mod state;
