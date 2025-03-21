// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! Console informant. Prints sync progress and block events. Runs on the calling thread.

use console::style;
use futures::prelude::*;
use futures_timer::Delay;
use log::{debug, info, log_enabled, trace};
use sc_client_api::{BlockchainEvents, UsageProvider};
use sc_network::NetworkStatusProvider;
use sc_network_sync::{SyncStatusProvider, SyncingService};
use sp_blockchain::HeaderMetadata;
use sp_runtime::traits::{Block as BlockT, Header};
use std::{
	collections::VecDeque,
	fmt::{Debug, Display},
	sync::Arc,
	time::Duration,
};

mod display;

/// Creates a stream that returns a new value every `duration`.
fn interval(duration: Duration) -> impl Stream<Item = ()> + Unpin {
	futures::stream::unfold((), move |_| Delay::new(duration).map(|_| Some(((), ())))).map(drop)
}

/// Builds the informant and returns a `Future` that drives the informant.
pub async fn build<B: BlockT, C, N>(client: Arc<C>, network: N, syncing: Arc<SyncingService<B>>)
where
	N: NetworkStatusProvider,
	C: UsageProvider<B> + HeaderMetadata<B> + BlockchainEvents<B>,
	<C as HeaderMetadata<B>>::Error: Display,
{
	let mut display = display::InformantDisplay::new();

	let client_1 = client.clone();

	let display_notifications = interval(Duration::from_millis(5000))
		.filter_map(|_| async {
			let net_status = network.status().await;
			let sync_status = syncing.status().await;
			let num_connected_peers = syncing.num_connected_peers();

			match (net_status, sync_status) {
				(Ok(net), Ok(sync)) => Some((net, sync, num_connected_peers)),
				_ => None,
			}
		})
		.for_each(move |(net_status, sync_status, num_connected_peers)| {
			let info = client_1.usage_info();
			if let Some(ref usage) = info.usage {
				trace!(target: "usage", "Usage statistics: {}", usage);
			} else {
				trace!(
					target: "usage",
					"Usage statistics not displayed as backend does not provide it",
				)
			}
			display.display(&info, net_status, sync_status, num_connected_peers);
			future::ready(())
		});

	futures::select! {
		() = display_notifications.fuse() => (),
		() = display_block_import(client).fuse() => (),
	};
}

/// Print the full hash when debug logging is enabled.
struct PrintFullHashOnDebugLogging<'a, H>(&'a H);

impl<H: Debug + Display> Display for PrintFullHashOnDebugLogging<'_, H> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		if log_enabled!(log::Level::Debug) {
			Debug::fmt(&self.0, f)
		} else {
			Display::fmt(&self.0, f)
		}
	}
}

async fn display_block_import<B: BlockT, C>(client: Arc<C>)
where
	C: UsageProvider<B> + HeaderMetadata<B> + BlockchainEvents<B>,
	<C as HeaderMetadata<B>>::Error: Display,
{
	let mut last_best = {
		let info = client.usage_info();
		Some((info.chain.best_number, info.chain.best_hash))
	};

	// Hashes of the last blocks we have seen at import.
	let mut last_blocks = VecDeque::new();
	let max_blocks_to_track = 100;
	let mut notifications = client.import_notification_stream();

	while let Some(n) = notifications.next().await {
		// detect and log reorganizations.
		if let Some((ref last_num, ref last_hash)) = last_best {
			if n.header.parent_hash() != last_hash && n.is_new_best {
				let maybe_ancestor =
					sp_blockchain::lowest_common_ancestor(&*client, *last_hash, n.hash);

				match maybe_ancestor {
					Ok(ref ancestor) if ancestor.hash != *last_hash => info!(
						"♻️  Reorg on #{},{} to #{},{}, common ancestor #{},{}",
						style(last_num).red().bold(),
						PrintFullHashOnDebugLogging(&last_hash),
						style(n.header.number()).green().bold(),
						PrintFullHashOnDebugLogging(&n.hash),
						style(ancestor.number).white().bold(),
						ancestor.hash,
					),
					Ok(_) => {},
					Err(e) => debug!("Error computing tree route: {}", e),
				}
			}
		}

		if n.is_new_best {
			last_best = Some((*n.header.number(), n.hash));
		}

		// If we already printed a message for a given block recently,
		// we should not print it again.
		if !last_blocks.contains(&n.hash) {
			last_blocks.push_back(n.hash);

			if last_blocks.len() > max_blocks_to_track {
				last_blocks.pop_front();
			}

			let best_indicator = if n.is_new_best { "🏆" } else { "🆕" };
			info!(
				target: "substrate",
				"{best_indicator} Imported #{} ({} → {})",
				style(n.header.number()).white().bold(),
				PrintFullHashOnDebugLogging(n.header.parent_hash()),
				PrintFullHashOnDebugLogging(&n.hash),
			);
		}
	}
}
