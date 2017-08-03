// Copyright (c) 2004-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

//! State for a single source control Repo

use std::fmt::{self, Debug};
use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;
use std::collections::{HashMap, HashSet};
use std::mem;

use bytes::Bytes;
use futures::{Async, BoxFuture, Future, IntoFuture, Poll, Stream, future, stream};
use futures_ext::StreamExt;

use slog::Logger;

use async_compression::CompressorType;
use bookmarks::Bookmarks;
use mercurial;
use mercurial::changeset;
use mercurial_bundles::{Bundle2EncodeBuilder, parts};
use mercurial_types::{Changeset, NULL_HASH, NodeHash, Parents, percent_encode};

use hgproto::{self, GetbundleArgs, HgCommandRes, HgCommands};

use errors::Result;

pub struct Repo {
    path: String,
    hgrepo: mercurial::RevlogRepo,
    #[allow(dead_code)]
    logger: Logger,
}

fn wireprotocaps() -> Vec<String> {
    vec![
        "lookup".to_string(),
        "known".to_string(),
        "getbundle".to_string(),
    ]
}

fn bundle2caps() -> String {
    let caps = hashmap! {
        "HG20" => vec![],
        "listkeys" => vec![],
        "changegroup" => vec!["02"],
    };

    let mut encodedcaps = vec![];

    for (key, value) in &caps {
        let encodedkey = key.to_string();
        if value.len() > 0 {
            let encodedvalue = value.join(",");
            encodedcaps.push([encodedkey, encodedvalue].join("="));
        } else {
            encodedcaps.push(encodedkey)
        }
    }

    percent_encode(&encodedcaps.join("\n"))
}

impl Repo {
    pub fn new<P: AsRef<Path>>(parent_logger: &Logger, path: P) -> Result<Self> {
        let path = path.as_ref();

        Ok(Repo {
            path: format!("{:?}", path),
            hgrepo: mercurial::RevlogRepo::open(&path)?,
            logger: parent_logger.new(o!("repo" => format!("{:?}", path))),
        })
    }
}

impl Debug for Repo {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "Repo({})", self.path)
    }
}

pub struct RepoClient {
    repo: Arc<Repo>,
    logger: Logger,
}

impl RepoClient {
    pub fn new(repo: Arc<Repo>, parent_logger: &Logger) -> Self {
        RepoClient {
            repo: repo,
            logger: parent_logger.new(o!()), // connection details?
        }
    }

    #[allow(dead_code)]
    pub fn get_logger(&self) -> &Logger {
        &self.logger
    }

    fn create_bundle(&self, args: GetbundleArgs) -> hgproto::Result<HgCommandRes<Bytes>> {
        let writer = Cursor::new(Vec::new());
        let mut bundle = Bundle2EncodeBuilder::new(writer);
        // Mercurial currently hangs while trying to read compressed bundles over the wire:
        // https://bz.mercurial-scm.org/show_bug.cgi?id=5646
        // TODO: possibly enable compression support once this is fixed.
        bundle.set_compressor_type(CompressorType::Uncompressed);

        // TODO: generalize this to other listkey types
        // (note: just calling &b"bookmarks"[..] doesn't work because https://fburl.com/0p0sq6kp)
        if args.listkeys.contains(&b"bookmarks".to_vec()) {
            let bookmarks = self.repo.hgrepo.bookmarks()?;
            let bookmark_names = bookmarks.keys();
            let items = bookmark_names.and_then(move |name| {
                // For each bookmark name, grab the corresponding value.
                bookmarks.get(&name).and_then(|result| {
                    // If the name somehow wasn't found, it's possible a race happened. where the
                    // bookmark was deleted from underneath. Skip it.
                    // Boxing is necessary here to make the match arms return the same types.
                    match result {
                        Some((hash, _version)) => {
                            // AsciiString doesn't currently implement AsRef<[u8]>, so switch to
                            // Vec which does
                            let hash: Vec<u8> = hash.to_hex().into();
                            Ok((name, hash)).into_future().boxed()
                        }
                        None => future::empty().boxed(),
                    }
                })
            });
            bundle.add_part(parts::listkey_part("bookmarks", items)?);
        }

        let encode_fut = bundle.build();

        Ok(
            encode_fut
                .map(|cursor| Bytes::from(cursor.into_inner()))
                .from_err()
                .boxed(),
        )
    }
}

impl HgCommands for RepoClient {
    // @wireprotocommand('between', 'pairs')
    fn between(&self, pairs: Vec<(NodeHash, NodeHash)>) -> HgCommandRes<Vec<Vec<NodeHash>>> {
        info!(self.logger, "between pairs {:?}", pairs);

        struct ParentStream<CS> {
            repo: Arc<Repo>,
            n: NodeHash,
            bottom: NodeHash,
            wait_cs: Option<CS>,
        };

        impl<CS> ParentStream<CS> {
            fn new(repo: Arc<Repo>, top: NodeHash, bottom: NodeHash) -> Self {
                ParentStream {
                    repo: repo,
                    n: top,
                    bottom: bottom,
                    wait_cs: None,
                }
            }
        }

        impl Stream for ParentStream<BoxFuture<changeset::RevlogChangeset, mercurial::Error>> {
            type Item = NodeHash;
            type Error = hgproto::Error;

            fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
                if self.n == self.bottom || self.n == NULL_HASH {
                    return Ok(Async::Ready(None));
                }

                self.wait_cs = self.wait_cs
                    .take()
                    .or_else(|| Some(self.repo.hgrepo.get_changeset_by_nodeid(&self.n)));
                let cs = try_ready!(self.wait_cs.as_mut().unwrap().poll());
                self.wait_cs = None; // got it

                let p = match cs.parents() {
                    &Parents::None => NULL_HASH,
                    &Parents::One(ref p) => *p,
                    &Parents::Two(ref p, _) => *p,
                };

                let prev_n = mem::replace(&mut self.n, p);

                Ok(Async::Ready(Some(prev_n)))
            }
        }

        // TODO(jsgf): do pairs in parallel?
        // TODO: directly return stream of streams
        let repo = self.repo.clone();
        stream::iter(pairs.into_iter().map(|p| Ok(p)))
            .and_then(move |(top, bottom)| {
                let mut f = 1;
                ParentStream::new(repo.clone(), top, bottom)
                    .enumerate()
                    .filter(move |&(i, _)| if i == f {
                        f *= 2;
                        true
                    } else {
                        false
                    })
                    .map(|(_, v)| v)
                    .collect()
            })
            .collect()
            .boxed()
    }

    // @wireprotocommand('changegroup', 'roots')
    fn changegroup(&self, roots: Vec<NodeHash>) -> HgCommandRes<()> {
        // TODO: streaming something
        info!(self.logger, "changegroup roots {:?}", roots);

        future::ok(()).boxed()
    }

    // @wireprotocommand('heads')
    fn heads(&self) -> HgCommandRes<HashSet<NodeHash>> {
        // Get a stream of heads and collect them into a HashSet
        // TODO: directly return stream of heads
        self.repo
            .hgrepo
            .get_heads()
            .collect()
            .from_err()
            .and_then(|v| Ok(v.into_iter().collect()))
            .boxed()
    }

    // @wireprotocommand('known', 'nodes *'), but the '*' is ignored
    fn known(&self, nodes: Vec<NodeHash>) -> HgCommandRes<Vec<bool>> {
        info!(self.logger, "known: {:?}", nodes);
        let known_futures: Vec<_> = nodes
            .iter()
            .map(|node| self.repo.hgrepo.changeset_exists(node))
            .collect();
        future::join_all(known_futures)
            .from_err::<hgproto::Error>()
            .boxed()
    }

    // @wireprotocommand('getbundle', '*')
    fn getbundle(&self, args: GetbundleArgs) -> HgCommandRes<Bytes> {
        info!(self.logger, "Getbundle: {:?}", args);

        match self.create_bundle(args) {
            Ok(res) => res,
            Err(err) => Err(err).into_future().boxed(),
        }
    }

    // @wireprotocommand('hello')
    fn hello(&self) -> HgCommandRes<HashMap<String, Vec<String>>> {
        info!(self.logger, "Hello -> capabilities");

        let mut res = HashMap::new();
        let mut caps = wireprotocaps();
        caps.push(format!("bundle2={}", bundle2caps()));
        res.insert("capabilities".to_string(), caps);

        future::ok(res).boxed()
    }

    // @wireprotocommand('unbundle', 'heads')
    fn unbundle(
        &self,
        heads: Vec<NodeHash>, /* , _stream: BoxStream<Vec<u8>, Error> */
    ) -> HgCommandRes<()> {
        info!(self.logger, "unbundle heads {:?}", heads);
        future::ok(()).boxed()
    }
}