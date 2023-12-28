use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use clap::{ArgAction, Parser};
use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use futures::{FutureExt, StreamExt};
use postgres_ffi::WAL_SEGMENT_SIZE;
use remote_storage::RemoteStorageConfig;
use safekeeper::control_file::FileStorage;
use safekeeper::safekeeper::SafeKeeperState;
use safekeeper::wal_storage::wal_file_paths;
use sd_notify::NotifyState;
use tokio::runtime::Handle;
use tokio::signal::unix::{signal, SignalKind};
use tokio::task::JoinError;
use toml_edit::Document;
use utils::id::{TenantId, TimelineId, TenantTimelineId};

use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use storage_broker::Uri;
use tokio::sync::mpsc;

use tracing::*;
use utils::pid_file;

use metrics::set_build_info_metric;
use safekeeper::defaults::{
    DEFAULT_HEARTBEAT_TIMEOUT, DEFAULT_HTTP_LISTEN_ADDR, DEFAULT_MAX_OFFLOADER_LAG_BYTES,
    DEFAULT_PG_LISTEN_ADDR,
};
use safekeeper::wal_service;
use safekeeper::GlobalTimelines;
use safekeeper::SafeKeeperConf;
use safekeeper::{broker, WAL_SERVICE_RUNTIME};
use safekeeper::{control_file, BROKER_RUNTIME};
use safekeeper::{http, WAL_REMOVER_RUNTIME};
use safekeeper::{remove_wal, WAL_BACKUP_RUNTIME};
use safekeeper::{wal_backup, HTTP_RUNTIME};
use storage_broker::DEFAULT_ENDPOINT;
use utils::auth::{JwtAuth, Scope, SwappableJwtAuth};
use utils::{
    id::NodeId,
    logging::{self, LogFormat},
    project_build_tag, project_git_version,
    sentry_init::init_sentry,
    tcp_listener,
};

const PID_FILE_NAME: &str = "safekeeper.pid";
const ID_FILE_NAME: &str = "safekeeper.id";

const CONTROL_FILE_NAME: &str = "safekeeper.control";

project_git_version!(GIT_VERSION);
project_build_tag!(BUILD_TAG);

const ABOUT: &str = r#"
Fixing the issue of some WAL files missing the prefix bytes.
"#;

#[derive(Parser)]
#[command(name = "Neon safekeeper", version = GIT_VERSION, about = ABOUT, long_about = None)]
struct Args {
    /// Path to the data2 directory.
    datafrom: Utf8PathBuf,
    /// Path to the data directory.
    datato: Utf8PathBuf,
    dryrun: bool,
}

struct TimelineDirInfo {
    ttid: TenantTimelineId,
    timeline_dir: Utf8PathBuf,
    control_file: SafeKeeperState,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // We want to allow multiple occurences of the same arg (taking the last) so
    // that neon_local could generate command with defaults + overrides without
    // getting 'argument cannot be used multiple times' error. This seems to be
    // impossible with pure Derive API, so convert struct to Command, modify it,
    // parse arguments, and then fill the struct back.
    let cmd = <Args as clap::CommandFactory>::command().args_override_self(true);
    let mut matches = cmd.get_matches();
    let mut args = <Args as clap::FromArgMatches>::from_arg_matches_mut(&mut matches)?;

    logging::init(
        LogFormat::from_config("plain")?,
        logging::TracingErrorLayerEnablement::Disabled,
        logging::Output::Stdout,
    )?;

    let all_timelines = read_all_timelines(&args.datafrom).await?;

    let wal_seg_size = WAL_SEGMENT_SIZE;

    for tli in all_timelines {
        assert!(tli.control_file.local_start_lsn == tli.control_file.timeline_start_lsn);
        info!("Found timeline {}, start_lsn={}, commit_lsn={}", tli.ttid, tli.control_file.local_start_lsn, tli.control_file.commit_lsn);
    
        let new_tli_dir = args.datato.join(tli.ttid.tenant_id.to_string()).join(tli.ttid.timeline_id.to_string());
        
        // check existence
        if !new_tli_dir.exists() {
            info!("Timeline {} does not exist in the target directory {}", tli.ttid, new_tli_dir);
            if args.dryrun {
                continue;
            }
            copy_directory(&tli, &new_tli_dir).await?;
            continue;
        }

        let new_tli = read_timeline(tli.ttid.clone(), new_tli_dir.as_path().as_std_path()).await?;
        if new_tli.control_file.local_start_lsn == tli.control_file.timeline_start_lsn {
            info!("Timeline {} is already fixed in the target directory {}", tli.ttid, new_tli_dir);
            continue;
        }

        let segnum = new_tli.control_file.local_start_lsn.segment_number(wal_seg_size);
        let valid_segnames = wal_file_paths(&tli.timeline_dir, segnum, wal_seg_size)?;
        let new_segnames = wal_file_paths(&new_tli.timeline_dir, segnum, wal_seg_size)?;

        info!(
            "Timeline {} has local_start_lsn={}, timeline_start_lsn={}, commit_lsn={} //// can be fixed with bytes from {} up to commit_lsn={}",
            new_tli.ttid,
            new_tli.control_file.local_start_lsn,
            new_tli.control_file.timeline_start_lsn,
            new_tli.control_file.commit_lsn,
            valid_segnames.0,
            tli.control_file.commit_lsn,
        );
        assert!(new_tli.control_file.timeline_start_lsn == tli.control_file.timeline_start_lsn);

        let new_segname = if new_segnames.0.exists() {
            new_segnames.0
        } else if new_segnames.1.exists() {
            new_segnames.1
        } else {
            info!("Segment {} was already deleted, nothing to backfill", new_segnames.0);
            continue;
        };

        let valid_segname = if valid_segnames.0.exists() {
            valid_segnames.0
        } else if valid_segnames.1.exists() {
            valid_segnames.1
        } else {
            panic!("Cannot find valid segment for timeline {}, this file doesn't exist {}", tli.ttid, valid_segnames.0);
        };

        if args.dryrun {
            continue;
        }

        info!("ACTION: Copying bytes from {} to {}", valid_segname, new_segname);
    }

    Ok(())
}

async fn read_all_timelines(dir: &Utf8Path) -> Result<Vec<TimelineDirInfo>> {
    info!("Reading all timelines from {:?}", dir);

    let mut timelines = Vec::new();
    for tenant_entry in fs::read_dir(dir).with_context(|| format!("Failed to read {:?}", dir))? {
        let tenant_entry = tenant_entry.with_context(|| format!("Failed to read {:?}", dir))?;
        let path = tenant_entry.path();
        if !path.is_dir() {
            info!("Skipping non-directory {:?}", path);
            continue;
        }
        let dirname = path.file_name().unwrap().to_str().unwrap();
        let tenant_id = TenantId::from_str(dirname);
        if tenant_id.is_err() {
            info!("Skipping non-tenant directory {:?}", path);
            continue;
        }
        let tenant_id = tenant_id.unwrap();

        for timeline_entry in fs::read_dir(&path).with_context(|| format!("Failed to read {:?}", path))?
        {
            let timeline_entry =
                timeline_entry.with_context(|| format!("Failed to read {:?}", path))?;
            let path = timeline_entry.path();
            if !path.is_dir() {
                info!("Skipping non-directory {:?}", path);
                continue;
            }
            let dirname = path.file_name().unwrap().to_str().unwrap();
            let timeline_id = TimelineId::from_str(dirname);
            if timeline_id.is_err() {
                info!("Skipping non-timeline directory {:?}", path);
                continue;
            }
            let timeline_id = timeline_id.unwrap();
            let ttid = TenantTimelineId::new(tenant_id, timeline_id);

            let tliinfo = read_timeline(ttid, &path).await?;
            timelines.push(tliinfo);
        }
    }
    Ok(timelines)
}

async fn read_timeline(ttid: TenantTimelineId, dir: &Path) -> Result<TimelineDirInfo> {
    let control_file_path = dir.join(CONTROL_FILE_NAME);
    let control_file = FileStorage::load_control_file(control_file_path)?;
    Ok(TimelineDirInfo {
        ttid,
        timeline_dir: Utf8PathBuf::from_path_buf(dir.to_path_buf()).expect("valid utf8"),
        control_file,
    })
}

async fn copy_directory(tli: &TimelineDirInfo, new_tli_dir: &Utf8Path) -> Result<()> {
    info!("ACTION: Copying timeline {} to {}", tli.ttid, new_tli_dir);
    // TODO: 
    Ok(())
}
