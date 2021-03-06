// Copyright 2018 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Tests for polkadot and consensus network.

use super::{PolkadotProtocol, Status, CurrentConsensus, Knowledge, Message, FullStatus};

use parking_lot::Mutex;
use polkadot_consensus::GenericStatement;
use polkadot_primitives::{Block, Hash, SessionKey};
use polkadot_primitives::parachain::{CandidateReceipt, HeadData, BlockData};
use substrate_primitives::H512;
use codec::Slicable;
use substrate_network::{PeerId, PeerInfo, ClientHandle, Context, message::Message as SubstrateMessage, message::Role, specialization::Specialization, generic_message::Message as GenericMessage};

use std::sync::Arc;
use futures::Future;

#[derive(Default)]
struct TestContext {
	disabled: Vec<PeerId>,
	disconnected: Vec<PeerId>,
	messages: Vec<(PeerId, SubstrateMessage<Block>)>,
}

impl Context<Block> for TestContext {
	fn client(&self) -> &ClientHandle<Block> {
		unimplemented!()
	}

	fn disable_peer(&mut self, peer: PeerId) {
		self.disabled.push(peer);
	}

	fn disconnect_peer(&mut self, peer: PeerId) {
		self.disconnected.push(peer);
	}

	fn peer_info(&self, _peer: PeerId) -> Option<PeerInfo<Block>> {
		unimplemented!()
	}

	fn send_message(&mut self, peer_id: PeerId, data: SubstrateMessage<Block>) {
		self.messages.push((peer_id, data))
	}
}

impl TestContext {
	fn has_message(&self, to: PeerId, message: Message) -> bool {
		use substrate_network::generic_message::Message as GenericMessage;

		let encoded = ::serde_json::to_vec(&message).unwrap();
		self.messages.iter().any(|&(ref peer, ref msg)| match msg {
			GenericMessage::ChainSpecific(ref data) => peer == &to && data == &encoded,
			_ => false,
		})
	}
}

fn make_status(status: &Status, roles: Vec<Role>) -> FullStatus {
	FullStatus {
		version: 1,
		roles,
		best_number: 0,
		best_hash: Default::default(),
		genesis_hash: Default::default(),
		chain_status: status.encode(),
	}
}

fn make_consensus(parent_hash: Hash, local_key: SessionKey) -> (CurrentConsensus, Arc<Mutex<Knowledge>>) {
	let knowledge = Arc::new(Mutex::new(Knowledge::new()));
	let c = CurrentConsensus {
		knowledge: knowledge.clone(),
		parent_hash,
		session_keys: Default::default(),
		local_session_key: local_key,
	};

	(c, knowledge)
}

fn on_message(protocol: &mut PolkadotProtocol, ctx: &mut TestContext, from: PeerId, message: Message) {
	let encoded = ::serde_json::to_vec(&message).unwrap();
	protocol.on_message(ctx, from, GenericMessage::ChainSpecific(encoded));
}

#[test]
fn sends_session_key() {
	let mut protocol = PolkadotProtocol::new();

	let peer_a = 1;
	let peer_b = 2;
	let parent_hash = [0; 32].into();
	let local_key = [1; 32].into();

	let status = Status { collating_for: None };

	{
		let mut ctx = TestContext::default();
		protocol.on_connect(&mut ctx, peer_a, make_status(&status, vec![Role::Authority]));
		assert!(ctx.messages.is_empty());
	}

	{
		let mut ctx = TestContext::default();
		let (consensus, _knowledge) = make_consensus(parent_hash, local_key);
		protocol.new_consensus(&mut ctx, consensus);

		assert!(ctx.has_message(peer_a, Message::SessionKey(parent_hash, local_key)));
	}

	{
		let mut ctx = TestContext::default();
		protocol.on_connect(&mut ctx, peer_b, make_status(&status, vec![Role::Authority]));
		assert!(ctx.has_message(peer_b, Message::SessionKey(parent_hash, local_key)));
	}
}

#[test]
fn fetches_from_those_with_knowledge() {
	let mut protocol = PolkadotProtocol::new();

	let peer_a = 1;
	let peer_b = 2;
	let parent_hash = [0; 32].into();
	let local_key = [1; 32].into();

	let block_data = BlockData(vec![1, 2, 3, 4]);
	let block_data_hash = block_data.hash();
	let candidate_receipt = CandidateReceipt {
		parachain_index: 5.into(),
		collator: [255; 32].into(),
		head_data: HeadData(vec![9, 9, 9]),
		signature: H512::from([1; 64]).into(),
		balance_uploads: Vec::new(),
		egress_queue_roots: Vec::new(),
		fees: 1_000_000,
		block_data_hash,
	};

	let candidate_hash = candidate_receipt.hash();
	let a_key = [3; 32].into();
	let b_key = [4; 32].into();

	let status = Status { collating_for: None };

	let (consensus, knowledge) = make_consensus(parent_hash, local_key);
	protocol.new_consensus(&mut TestContext::default(), consensus);

	knowledge.lock().note_statement(a_key, &GenericStatement::Valid(candidate_hash));
	let recv = protocol.fetch_block_data(&mut TestContext::default(), &candidate_receipt, parent_hash);

	// connect peer A
	{
		let mut ctx = TestContext::default();
		protocol.on_connect(&mut ctx, peer_a, make_status(&status, vec![Role::Authority]));
		assert!(ctx.has_message(peer_a, Message::SessionKey(parent_hash, local_key)));
	}

	// peer A gives session key and gets asked for data.
	{
		let mut ctx = TestContext::default();
		on_message(&mut protocol, &mut ctx, peer_a, Message::SessionKey(parent_hash, a_key));
		assert!(ctx.has_message(peer_a, Message::RequestBlockData(1, candidate_hash)));
	}

	knowledge.lock().note_statement(b_key, &GenericStatement::Valid(candidate_hash));

	// peer B connects and sends session key. request already assigned to A
	{
		let mut ctx = TestContext::default();
		protocol.on_connect(&mut ctx, peer_b, make_status(&status, vec![Role::Authority]));
		on_message(&mut protocol, &mut ctx, peer_b, Message::SessionKey(parent_hash, b_key));
		assert!(!ctx.has_message(peer_b, Message::RequestBlockData(2, candidate_hash)));

	}

	// peer A disconnects, triggering reassignment
	{
		let mut ctx = TestContext::default();
		protocol.on_disconnect(&mut ctx, peer_a);
		assert!(ctx.has_message(peer_b, Message::RequestBlockData(2, candidate_hash)));
	}

	// peer B comes back with block data.
	{
		let mut ctx = TestContext::default();
		on_message(&mut protocol, &mut ctx, peer_b, Message::BlockData(2, Some(block_data.clone())));
		drop(protocol);
		assert_eq!(recv.wait().unwrap(), block_data);
	}
}
