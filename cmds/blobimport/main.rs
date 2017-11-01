// Copyright (c) 2004-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

#![deny(warnings)]
#![feature(conservative_impl_trait)]

extern crate bincode;
extern crate bytes;
extern crate clap;
#[macro_use]
extern crate error_chain;
extern crate futures;
extern crate futures_cpupool;
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate slog;
#[macro_use]
extern crate slog_glog_fmt;
extern crate slog_term;
extern crate tokio_core;

extern crate blobrepo;
extern crate blobstore;
extern crate fileblob;
extern crate fileheads;
extern crate futures_ext;
extern crate heads;
extern crate manifoldblob;
extern crate mercurial;
extern crate mercurial_types;
extern crate rocksblob;
extern crate rocksdb;
extern crate services;
#[macro_use]
extern crate stats;

mod convert;
mod errors;
mod manifest;

use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::sync_channel;
use std::thread;
use std::time::Duration;

use bytes::Bytes;
use clap::{App, Arg, ArgMatches};
use futures::{stream, Future, IntoFuture, Stream};
use futures_cpupool::CpuPool;
use slog::{Drain, Level, Logger};
use slog_glog_fmt::default_drain as glog_drain;
use tokio_core::reactor::{Core, Remote};

use blobrepo::BlobChangeset;
use blobstore::{Blobstore, RetryingBlobstore};
use fileblob::Fileblob;
use fileheads::FileHeads;
use futures_ext::{BoxFuture, FutureExt};
use manifoldblob::ManifoldBlob;
use mercurial::RevlogRepo;
use rocksblob::Rocksblob;

use errors::*;

const DEFAULT_MANIFOLD_BUCKET: &str = "mononoke_prod";

define_stats! {
    prefix = "blobimport";
    changesets: timeseries(RATE, SUM),
    heads: timeseries(RATE, SUM),
}

#[derive(Debug, Eq, PartialEq)]
enum BlobstoreType {
    Files,
    Rocksdb,
    Manifold(String),
}

type BBlobstore = Arc<
    Blobstore<
        Key = String,
        ValueIn = Bytes,
        ValueOut = Vec<u8>,
        Error = Error,
        GetBlob = BoxFuture<Option<Vec<u8>>, Error>,
        PutBlob = BoxFuture<(), Error>,
    >
        + Sync,
>;

fn _assert_clone<T: Clone>(_: &T) {}
fn _assert_send<T: Send>(_: &T) {}
fn _assert_static<T: 'static>(_: &T) {}
fn _assert_blobstore<T: Blobstore>(_: &T) {}

pub(crate) enum BlobstoreEntry {
    ManifestEntry((String, Bytes)),
    Changeset(BlobChangeset),
}

fn run_blobimport<In: AsRef<Path> + Debug, Out: AsRef<Path> + Debug>(
    input: In,
    output: Out,
    blobtype: BlobstoreType,
    logger: &Logger,
    postpone_compaction: bool,
    channel_size: usize,
    commits_limit: Option<usize>,
) -> Result<()>
where
    In: AsRef<Path>,
    Out: AsRef<Path>,
{
    let core = Core::new()?;
    let cpupool = Arc::new(CpuPool::new_num_cpus());

    info!(logger, "Opening headstore: {:?}", output);
    let headstore = open_headstore(&output, &cpupool)?;

    if let BlobstoreType::Manifold(ref bucket) = blobtype {
        info!(logger, "Using ManifoldBlob with bucket: {:?}", bucket);
    } else {
        info!(logger, "Opening blobstore: {:?}", output);
    }
    let output = output.as_ref().to_path_buf();

    let (sender, recv) = sync_channel::<BlobstoreEntry>(channel_size);
    // Separate thread that does all blobstore operations. Other worker threads send parsed revlog
    // data to this thread.
    let iothread = thread::Builder::new()
        .name("iothread".to_owned())
        .spawn(move || {
            let receiverstream = stream::iter_ok::<_, ()>(recv);
            let mut core = Core::new().expect("cannot create core in iothread");
            let blobstore = open_blobstore(output, blobtype, &core.remote(), postpone_compaction)?;
            // Filter only manifest entries, because changeset entries should be unique
            let mut inserted_manifest_entries = std::collections::HashSet::new();
            let stream = receiverstream
                .map(move |sender_helper| match sender_helper {
                    BlobstoreEntry::Changeset(bcs) => {
                        bcs.save(blobstore.clone()).from_err().boxify()
                    }
                    BlobstoreEntry::ManifestEntry((key, value)) => {
                        if inserted_manifest_entries.insert(key.clone()) {
                            blobstore.put(key, value)
                        } else {
                            Ok(()).into_future().boxify()
                        }
                    }
                })
                .map_err(|_| Error::from("error happened"))
                .buffer_unordered(channel_size);
            core.run(stream.for_each(|_| Ok(())))
        })
        .expect("cannot start iothread");

    let repo = open_repo(&input)?;

    info!(logger, "Converting: {:?}", input);
    let convert_context = convert::ConvertContext {
        repo,
        sender,
        headstore,
        core,
        cpupool,
        logger: logger.clone(),
        commits_limit: commits_limit,
    };
    let res = convert_context.convert();
    iothread.join().expect("failed to join io thread")?;
    res
}

fn open_repo<P: AsRef<Path>>(input: P) -> Result<RevlogRepo> {
    let mut input = PathBuf::from(input.as_ref());
    if !input.exists() || !input.is_dir() {
        bail!("input {:?} doesn't exist or isn't a dir", input);
    }
    input.push(".hg");

    let revlog = RevlogRepo::open(input)?;

    Ok(revlog)
}

fn open_headstore<P: AsRef<Path>>(heads: P, pool: &Arc<CpuPool>) -> Result<FileHeads<String>> {
    let mut heads = PathBuf::from(heads.as_ref());

    heads.push("heads");
    let headstore = fileheads::FileHeads::create_with_pool(heads, pool.clone())?;

    Ok(headstore)
}

fn open_blobstore(
    mut output: PathBuf,
    ty: BlobstoreType,
    remote: &Remote,
    postpone_compaction: bool,
) -> Result<BBlobstore> {
    output.push("blobs");

    let blobstore = match ty {
        BlobstoreType::Files => Fileblob::<_, Bytes>::create(output)
            .map_err(Error::from)
            .chain_err::<_, Error>(|| "Failed to open file blob store".into())?
            .arced(),
        BlobstoreType::Rocksdb => {
            let options = rocksdb::Options::new()
                .create_if_missing(true)
                .disable_auto_compaction(postpone_compaction);
            Rocksblob::open_with_options(output, options)
                .map_err(Error::from)
                .chain_err::<_, Error>(|| "Failed to open rocksdb blob store".into())?
                .arced()
        }
        BlobstoreType::Manifold(bucket) => {
            let mb: ManifoldBlob<String, Bytes> = ManifoldBlob::new_may_panic(bucket, remote);
            let rmb: RetryingBlobstore<
                String,
                Bytes,
                Vec<u8>,
                Error,
                manifoldblob::Error,
            > = RetryingBlobstore::new(
                mb.arced(),
                remote,
                Arc::new(|_| None),
                Arc::new(|attempt| if attempt > 3 {
                    None
                } else {
                    // 100ms 400ms 1.6s 6.4s
                    Some(Duration::from_millis(100 * 4u64.pow(attempt as u32)))
                }),
            );
            rmb.arced()
        }
    };

    _assert_clone(&blobstore);
    _assert_send(&blobstore);
    _assert_static(&blobstore);
    _assert_blobstore(&blobstore);

    Ok(blobstore)
}

fn setup_app<'a, 'b>() -> App<'a, 'b> {
    App::new("revlog to blob importer")
        .version("0.0.0")
        .about("make blobs")
        .args_from_usage(
            r#"
            <INPUT>                  'input revlog repo'
            <OUTPUT>                 'output blobstore RepoCtx'

            -p, --port [PORT]        'if provided the thrift server will start on this port'

            --postpone-compaction    '(rocksdb only) postpone auto compaction while importing'

            -d, --debug              'print debug level output'
            --channel-size [SIZE]    'channel size between worker and io threads. Default: 1000'
            --commits-limit [LIMIT]  'import only LIMIT first commits from revlog repo'
        "#,
        )
        .arg(
            Arg::with_name("blobstore")
                .long("blobstore")
                .short("B")
                .takes_value(true)
                .possible_values(&["files", "rocksdb", "manifold"])
                .required(true)
                .help("blobstore type"),
        )
        .arg(
            Arg::with_name("bucket")
                .long("bucket")
                .takes_value(true)
                .help("bucket to use for manifold blobstore"),
        )
}

fn start_thrift_service<'a>(logger: &Logger, matches: &ArgMatches<'a>) -> Result<()> {
    let port = match matches.value_of("port") {
        None => return Ok(()),
        Some(port) => port.parse().expect("Failed to parse port as number"),
    };

    info!(logger, "Initializing thrift server on port {}", port);

    thread::Builder::new()
        .name("thrift_service".to_owned())
        .spawn(move || {
            services::run_service_framework(
                "mononoke_server",
                port,
                0, // Disables separate status http server
            ).expect("failure while running thrift service framework")
        })
        .map(|_| ()) // detaches the thread
        .map_err(Error::from)
}

fn start_stats() -> Result<()> {
    thread::Builder::new()
        .name("stats_aggregation".to_owned())
        .spawn(move || {
            let mut core = Core::new().expect("failed to create tokio core");
            let scheduler = stats::schedule_stats_aggregation(&core.handle())
                .expect("failed to create stats aggregation scheduler");
            core.run(scheduler).expect("stats scheduler failed");
            // stats scheduler shouldn't finish successfully
            unreachable!()
        })?; // thread detached
    Ok(())
}

fn main() {
    let matches = setup_app().get_matches();

    let root_log = {
        let level = if matches.is_present("debug") {
            Level::Debug
        } else {
            Level::Info
        };

        let drain = glog_drain().filter_level(level).fuse();
        slog::Logger::root(drain, o![])
    };

    fn run<'a>(root_log: &Logger, matches: ArgMatches<'a>) -> Result<()> {
        start_thrift_service(&root_log, &matches)?;
        start_stats()?;

        let input = matches.value_of("INPUT").unwrap();
        let output = matches.value_of("OUTPUT").unwrap();
        let bucket = matches
            .value_of("bucket")
            .unwrap_or(DEFAULT_MANIFOLD_BUCKET);

        let blobtype = match matches.value_of("blobstore").unwrap() {
            "files" => BlobstoreType::Files,
            "rocksdb" => BlobstoreType::Rocksdb,
            "manifold" => BlobstoreType::Manifold(bucket.to_string()),
            bad => panic!("unexpected blobstore type {}", bad),
        };

        let postpone_compaction = matches.is_present("postpone-compaction");

        let channel_size: usize = matches
            .value_of("channel-size")
            .map(|size| {
                size.parse().expect("channel-size must be positive integer")
            })
            .unwrap_or(1000);

        run_blobimport(
            input,
            output,
            blobtype,
            &root_log,
            postpone_compaction,
            channel_size,
            matches.value_of("commits-limit").map(|size|
                size.parse().expect("commits-limit must be positive integer")
            ),
        )?;

        if matches.value_of("blobstore").unwrap() == "rocksdb" && postpone_compaction {
            let options = rocksdb::Options::new().create_if_missing(false);
            let rocksdb = rocksdb::Db::open(Path::new(output).join("blobs"), options)
                .expect("can't open rocksdb");
            info!(root_log, "compaction started");
            rocksdb.compact_range(&[], &[]);
            info!(root_log, "compaction finished");
        }

        Ok(())
    }

    if let Err(e) = run(&root_log, matches) {
        error!(root_log, "Blobimport failed"; e);
        std::process::exit(1);
    }
}
