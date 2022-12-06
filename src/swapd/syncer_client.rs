// Copyright 2020-2022 Farcaster Devs & LNP/BP Standards Association
//
// Use of this source code is governed by an MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT.

use crate::{
    bus::ServiceBus,
    service::{Endpoints, LogStyle},
    syncerd::{
        Abort, AddressAddendum, Boolean, BroadcastTransaction, BtcAddressAddendum, GetTx,
        SweepAddress, SweepAddressAddendum, SweepBitcoinAddress, SweepMoneroAddress, TaskTarget,
        TransactionBroadcasted, WatchAddress, WatchEstimateFee, WatchHeight, WatchTransaction,
        XmrAddressAddendum,
    },
    Error,
};
use bitcoin::{consensus::Decodable, Txid};
use farcaster_core::{blockchain::Blockchain, swap::SwapId, transaction::TxLabel};
use std::collections::{HashMap, HashSet};

use crate::{
    bus::sync::SyncMsg,
    bus::BusMsg,
    syncerd::{Task, TaskId},
    ServiceId,
};

pub struct SyncerTasks {
    pub counter: u32,
    pub watched_txs: HashMap<TaskId, TxLabel>,
    pub final_txs: HashMap<TxLabel, bool>,
    pub watched_addrs: HashMap<TaskId, TxLabel>,
    pub retrieving_txs: HashMap<TaskId, (TxLabel, Task)>,
    pub broadcasting_txs: HashSet<TaskId>,
    pub sweeping_addr: Option<TaskId>,
    // external address: needed to subscribe for buy (bob) or refund (alice) address_txs
    pub txids: HashMap<TxLabel, Txid>,
    pub tasks: HashMap<TaskId, Task>,
}

impl SyncerTasks {
    pub fn new_taskid(&mut self) -> TaskId {
        self.counter += 1;
        TaskId(self.counter)
    }
}

pub struct SyncerState {
    pub swap_id: SwapId,
    pub tasks: SyncerTasks,
    pub bitcoin_height: u64,
    pub monero_height: u64,
    pub confirmation_bound: u32,
    pub lock_tx_confs: Option<SyncMsg>,
    pub cancel_tx_confs: Option<SyncMsg>,
    pub network: farcaster_core::blockchain::Network,
    pub bitcoin_syncer: ServiceId,
    pub monero_syncer: ServiceId,
    pub monero_amount: monero::Amount,
    pub bitcoin_amount: bitcoin::Amount,
    pub xmr_addr_addendum: Option<XmrAddressAddendum>,
    pub confirmations: HashMap<TxLabel, Option<u32>>,
    pub awaiting_funding: bool,
    pub btc_fee_estimate_sat_per_kvb: Option<u64>,
}
impl SyncerState {
    pub fn task_lifetime(&self, blockchain: Blockchain) -> u64 {
        let height = self.height(blockchain);
        if height > 0 {
            height + 500
        } else {
            u64::MAX
        }
    }
    pub fn bitcoin_syncer(&self) -> ServiceId {
        self.bitcoin_syncer.clone()
    }
    pub fn monero_syncer(&self) -> ServiceId {
        self.monero_syncer.clone()
    }
    pub fn height(&self, blockchain: Blockchain) -> u64 {
        match blockchain {
            Blockchain::Bitcoin => self.bitcoin_height,
            Blockchain::Monero => self.monero_height,
        }
    }
    pub fn handle_height_change(&mut self, new_height: u64, blockchain: Blockchain) {
        let height = match blockchain {
            Blockchain::Bitcoin => &mut self.bitcoin_height,
            Blockchain::Monero => &mut self.monero_height,
        };
        if &new_height > height {
            debug!("{} new height {}", blockchain, &new_height);
            *height = new_height;
        } else {
            warn!("block height did not increment, maybe syncer sends multiple events");
        }
    }
    pub fn abort_task(&mut self, id: TaskId) -> Task {
        Task::Abort(Abort {
            task_target: TaskTarget::TaskId(id),
            respond: Boolean::False,
        })
    }

    pub fn estimate_fee_btc(&mut self) -> Task {
        let id = self.tasks.new_taskid();
        let task = Task::WatchEstimateFee(WatchEstimateFee {
            id,
            lifetime: self.task_lifetime(Blockchain::Bitcoin),
        });
        self.tasks.tasks.insert(id, task.clone());
        task
    }

    pub fn watch_tx_btc(&mut self, txid: Txid, tx_label: TxLabel) -> Task {
        let id = self.tasks.new_taskid();
        self.tasks.watched_txs.insert(id, tx_label);
        self.tasks.txids.insert(tx_label, txid);
        info!(
            "{} | Watching {} transaction ({})",
            self.swap_id.swap_id(),
            tx_label.label(),
            txid.tx_hash()
        );
        let task = Task::WatchTransaction(WatchTransaction {
            id,
            lifetime: self.task_lifetime(Blockchain::Bitcoin),
            hash: txid.to_vec(),
            confirmation_bound: self.confirmation_bound,
        });
        self.tasks.tasks.insert(id, task.clone());
        task
    }
    pub fn is_watched_tx(&self, tx_label: &TxLabel) -> bool {
        self.tasks.watched_txs.values().any(|tx| tx == tx_label)
    }
    pub fn watch_tx_xmr(&mut self, hash: Vec<u8>, tx_label: TxLabel) -> Task {
        let id = self.tasks.new_taskid();
        self.tasks.watched_txs.insert(id, tx_label);
        info!(
            "{} | Watching {} transaction ({})",
            self.swap_id.swap_id(),
            tx_label.label(),
            hex::encode(&hash).tx_hash(),
        );
        debug!("Watching transaction {} with {}", hex::encode(&hash), id);
        let task = Task::WatchTransaction(WatchTransaction {
            id,
            lifetime: self.task_lifetime(Blockchain::Monero),
            hash,
            confirmation_bound: self.confirmation_bound,
        });
        self.tasks.tasks.insert(id, task.clone());
        task
    }
    pub fn retrieve_tx_btc(&mut self, txid: Txid, tx_label: TxLabel) -> Task {
        let id = self.tasks.new_taskid();
        let task = Task::GetTx(GetTx {
            id,
            hash: txid.to_vec(),
        });
        self.tasks
            .retrieving_txs
            .insert(id, (tx_label, task.clone()));
        self.tasks.tasks.insert(id, task.clone());
        task
    }
    pub fn watch_addr_btc(&mut self, address: bitcoin::Address, tx_label: TxLabel) -> Task {
        let id = self.tasks.new_taskid();
        let from_height = self.from_height(Blockchain::Bitcoin, 6);
        self.tasks.watched_addrs.insert(id, tx_label);
        info!(
            "{} | Watching address {} for {} transaction",
            self.swap_id.swap_id(),
            address.addr(),
            tx_label.label(),
        );
        let addendum = BtcAddressAddendum {
            from_height,
            address,
        };
        let task = Task::WatchAddress(WatchAddress {
            id,
            lifetime: self.task_lifetime(Blockchain::Bitcoin),
            addendum: AddressAddendum::Bitcoin(addendum),
            include_tx: Boolean::True,
        });
        self.tasks.tasks.insert(id, task.clone());
        task
    }

    pub fn is_watched_addr(&self, tx_label: &TxLabel) -> bool {
        self.tasks.watched_addrs.values().any(|tx| tx == tx_label)
    }
    pub fn from_height(&self, blockchain: Blockchain, delta: u64) -> u64 {
        let height = self.height(blockchain);
        let delta = if height > delta { delta } else { height };
        height - delta
    }

    pub fn watch_addr_xmr(
        &mut self,
        spend: monero::PublicKey,
        view: monero::PrivateKey,
        tx_label: TxLabel,
        from_height: Option<u64>,
    ) -> Task {
        debug!(
            "{} | Address's secret view key for {}: {}",
            self.swap_id.bright_blue_italic(),
            tx_label.bright_white_bold(),
            view.bright_white_italic()
        );
        debug!(
            "{} | Address's public spend key for {}: {}",
            self.swap_id.bright_blue_italic(),
            tx_label.bright_white_bold(),
            spend.bright_white_italic()
        );
        let viewpair = monero::ViewPair { spend, view };
        let address = monero::Address::from_viewpair(self.network.into(), &viewpair);
        let from_height = from_height.unwrap_or(self.from_height(Blockchain::Monero, 20));
        let addendum = XmrAddressAddendum {
            spend_key: spend,
            view_key: view,
            from_height,
        };

        self.xmr_addr_addendum = Some(addendum.clone());

        let id = self.tasks.new_taskid();
        self.tasks.watched_addrs.insert(id, tx_label);

        info!(
            "{} | Watching {} on address {}",
            self.swap_id.swap_id(),
            tx_label.label(),
            address.addr(),
        );

        let watch_addr = WatchAddress {
            id,
            lifetime: self.task_lifetime(Blockchain::Monero),
            addendum: AddressAddendum::Monero(addendum),
            include_tx: Boolean::False,
        };
        let task = Task::WatchAddress(watch_addr);
        self.tasks.tasks.insert(id, task.clone());
        task
    }

    pub fn watch_height(&mut self, blockchain: Blockchain) -> Task {
        let id = self.tasks.new_taskid();
        trace!("Watch height {}", blockchain);
        let task = Task::WatchHeight(WatchHeight {
            id,
            lifetime: self.task_lifetime(blockchain),
        });
        self.tasks.tasks.insert(id, task.clone());
        task
    }

    pub fn sweep_btc(&mut self, addendum: SweepBitcoinAddress, retry: bool) -> Task {
        let id = self.tasks.new_taskid();
        self.tasks.sweeping_addr = Some(id);
        let lifetime = self.task_lifetime(Blockchain::Bitcoin);
        let sweep_task = SweepAddress {
            id,
            lifetime,
            addendum: SweepAddressAddendum::Bitcoin(addendum),
            retry,
        };
        let task = Task::SweepAddress(sweep_task);
        self.tasks.tasks.insert(id, task.clone());
        task
    }

    pub fn sweep_xmr(&mut self, addendum: SweepMoneroAddress, retry: bool) -> Task {
        let id = self.tasks.new_taskid();
        self.tasks.sweeping_addr = Some(id);
        let lifetime = self.task_lifetime(Blockchain::Monero);
        let sweep_task = SweepAddress {
            id,
            lifetime,
            addendum: SweepAddressAddendum::Monero(addendum),
            retry,
        };
        let task = Task::SweepAddress(sweep_task);
        self.tasks.tasks.insert(id, task.clone());
        task
    }

    pub fn broadcast(&mut self, tx: bitcoin::Transaction) -> Task {
        let id = self.tasks.new_taskid();
        let task = Task::BroadcastTransaction(BroadcastTransaction {
            id,
            tx: bitcoin::consensus::serialize(&tx),
            broadcast_after_height: None,
        });
        self.tasks.tasks.insert(id, task.clone());
        self.tasks.broadcasting_txs.insert(id);
        task
    }
    pub fn transaction_broadcasted(&mut self, event: &TransactionBroadcasted) {
        self.tasks.broadcasting_txs.remove(&event.id);
        self.tasks.tasks.remove(&event.id);
    }
    pub fn pending_broadcast_txs(&self) -> Vec<bitcoin::Transaction> {
        self.tasks
            .broadcasting_txs
            .iter()
            .filter_map(|id| {
                if let Task::BroadcastTransaction(broadcast_tx) = self.tasks.tasks.get(id)? {
                    Some(
                        bitcoin::Transaction::consensus_decode(std::io::Cursor::new(
                            broadcast_tx.tx.clone(),
                        ))
                        .ok()?,
                    )
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn acc_lock_watched(&self) -> bool {
        self.tasks
            .watched_addrs
            .values()
            .any(|&x| x == TxLabel::AccLock)
    }
    pub fn handle_tx_confs(
        &mut self,
        id: &TaskId,
        confirmations: &Option<u32>,
        swapid: SwapId,
        finality_thr: u32,
    ) {
        if let Some(txlabel) = self.tasks.watched_txs.get(id) {
            if !self.tasks.final_txs.contains_key(txlabel)
                && confirmations.is_some()
                && confirmations.unwrap() >= finality_thr
            {
                info!(
                    "{} | Tx {} {} with {} {}",
                    self.swap_id.swap_id(),
                    txlabel.label(),
                    "final".bright_green_bold(),
                    confirmations.unwrap().bright_green_bold(),
                    "confirmations".bright_green_bold()
                );
                self.tasks.final_txs.insert(*txlabel, true);
            } else if let Some(finality) = self.tasks.final_txs.get(txlabel) {
                info!(
                    "{} | Tx {} {}",
                    self.swap_id.swap_id(),
                    txlabel.label(),
                    if *finality {
                        "final".bright_green_bold()
                    } else {
                        "non-final".red_bold()
                    },
                );
            } else {
                match confirmations {
                    Some(0) => {
                        info!(
                            "{} | Tx {} on mempool but hasn't been mined",
                            swapid.swap_id(),
                            txlabel.label()
                        );
                    }
                    Some(confs) => {
                        info!(
                            "{} | Tx {} mined with {} {}",
                            swapid.swap_id(),
                            txlabel.label(),
                            confs.bright_green_bold(),
                            "confirmations".bright_green_bold(),
                        )
                    }
                    None => {
                        info!(
                            "{} | Tx {} not on the mempool",
                            swapid.swap_id(),
                            txlabel.label()
                        );
                    }
                }
            }
            self.confirmations
                .insert(txlabel.clone(), confirmations.clone());
        } else {
            error!(
                "received event with unknown transaction and task id {}",
                &id
            )
        }
    }
    pub fn watch_fee_and_height(&mut self, endpoints: &mut Endpoints) -> Result<(), Error> {
        let identity = ServiceId::Swap(self.swap_id.clone());
        let task = self.estimate_fee_btc();
        endpoints.send_to(
            ServiceBus::Sync,
            identity.clone(),
            self.bitcoin_syncer(),
            BusMsg::Sync(SyncMsg::Task(task)),
        )?;
        let watch_height_btc_task = self.watch_height(Blockchain::Bitcoin);
        endpoints.send_to(
            ServiceBus::Sync,
            identity.clone(),
            self.bitcoin_syncer(),
            BusMsg::Sync(SyncMsg::Task(watch_height_btc_task)),
        )?;
        // assumes xmr syncer will be up as well at this point
        let watch_height_xmr_task = self.watch_height(Blockchain::Monero);
        endpoints.send_to(
            ServiceBus::Sync,
            identity,
            self.monero_syncer(),
            BusMsg::Sync(SyncMsg::Task(watch_height_xmr_task)),
        )?;
        Ok(())
    }

    pub fn get_confs(&self, label: TxLabel) -> Option<u32> {
        self.confirmations.get(&label).map(|c| c.clone()).flatten()
    }
}

pub fn log_tx_seen(swap_id: SwapId, txlabel: &TxLabel, txid: &Txid) {
    info!(
        "{} | {} transaction ({}) in mempool or blockchain, handling with swap wallet",
        swap_id.swap_id(),
        txlabel.label(),
        txid.tx_hash(),
    );
}

pub fn log_tx_created(swap_id: SwapId, txlabel: TxLabel) {
    info!(
        "{} | {} transaction created",
        swap_id.swap_id(),
        txlabel.label(),
    );
}
