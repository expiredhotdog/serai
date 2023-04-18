use core::fmt;
use std::collections::{VecDeque, HashMap};

use rand_core::OsRng;

use scale::Encode;

use group::GroupEncoding;
use frost::{
  curve::Ristretto,
  ThresholdKeys,
  sign::{
    Writable, PreprocessMachine, SignMachine, SignatureMachine, AlgorithmMachine,
    AlgorithmSignMachine, AlgorithmSignatureMachine,
  },
};
use frost_schnorrkel::Schnorrkel;

use log::{info, debug, warn};

use serai_client::{
  primitives::BlockHash,
  in_instructions::primitives::{Batch, SignedBatch},
};

use messages::{sign::SignId, coordinator::*};
use crate::{DbTxn, Db};

#[derive(Debug)]
pub enum SubstrateSignerEvent {
  ProcessorMessage(ProcessorMessage),
  SignedBatch(SignedBatch),
}

#[derive(Debug)]
struct SubstrateSignerDb<D: Db>(D);
impl<D: Db> SubstrateSignerDb<D> {
  fn sign_key(dst: &'static [u8], key: impl AsRef<[u8]>) -> Vec<u8> {
    D::key(b"SUBSTRATE_SIGNER", dst, key)
  }

  fn completed_key(id: [u8; 32]) -> Vec<u8> {
    Self::sign_key(b"completed", id)
  }
  fn complete(txn: &mut D::Transaction<'_>, id: [u8; 32]) {
    txn.put(Self::completed_key(id), [1]);
  }
  fn completed(&self, id: [u8; 32]) -> bool {
    self.0.get(Self::completed_key(id)).is_some()
  }

  fn attempt_key(id: &SignId) -> Vec<u8> {
    Self::sign_key(b"attempt", bincode::serialize(id).unwrap())
  }
  fn attempt(txn: &mut D::Transaction<'_>, id: &SignId) {
    txn.put(Self::attempt_key(id), []);
  }
  fn has_attempt(&mut self, id: &SignId) -> bool {
    self.0.get(Self::attempt_key(id)).is_some()
  }

  fn save_batch(txn: &mut D::Transaction<'_>, batch: &SignedBatch) {
    txn.put(Self::sign_key(b"batch", batch.batch.block), batch.encode());
  }
}

pub struct SubstrateSigner<D: Db> {
  db: SubstrateSignerDb<D>,

  keys: ThresholdKeys<Ristretto>,

  signable: HashMap<[u8; 32], Batch>,
  attempt: HashMap<[u8; 32], u32>,
  preprocessing: HashMap<[u8; 32], AlgorithmSignMachine<Ristretto, Schnorrkel>>,
  signing: HashMap<[u8; 32], AlgorithmSignatureMachine<Ristretto, Schnorrkel>>,

  pub events: VecDeque<SubstrateSignerEvent>,
}

impl<D: Db> fmt::Debug for SubstrateSigner<D> {
  fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
    fmt
      .debug_struct("SubstrateSigner")
      .field("signable", &self.signable)
      .field("attempt", &self.attempt)
      .finish_non_exhaustive()
  }
}

impl<D: Db> SubstrateSigner<D> {
  pub fn new(db: D, keys: ThresholdKeys<Ristretto>) -> SubstrateSigner<D> {
    SubstrateSigner {
      db: SubstrateSignerDb(db),

      keys,

      signable: HashMap::new(),
      attempt: HashMap::new(),
      preprocessing: HashMap::new(),
      signing: HashMap::new(),

      events: VecDeque::new(),
    }
  }

  fn verify_id(&self, id: &SignId) -> Result<(), ()> {
    // Check the attempt lines up
    match self.attempt.get(&id.id) {
      // If we don't have an attempt logged, it's because the coordinator is faulty OR because we
      // rebooted OR we detected the signed batch on chain
      // The latter is the expected flow for batches not actively being participated in
      None => {
        warn!("not attempting batch {} #{}", hex::encode(id.id), id.attempt);
        Err(())?;
      }
      Some(attempt) => {
        if attempt != &id.attempt {
          warn!(
            "sent signing data for batch {} #{} yet we have attempt #{}",
            hex::encode(id.id),
            id.attempt,
            attempt
          );
          Err(())?;
        }
      }
    }

    Ok(())
  }

  async fn attempt(&mut self, id: [u8; 32], attempt: u32) {
    // See above commentary for why this doesn't emit SignedBatch
    if self.db.completed(id) {
      return;
    }

    // Check if we're already working on this attempt
    if let Some(curr_attempt) = self.attempt.get(&id) {
      if curr_attempt >= &attempt {
        warn!(
          "told to attempt {} #{} yet we're already working on {}",
          hex::encode(id),
          attempt,
          curr_attempt
        );
        return;
      }
    }

    // Start this attempt
    if !self.signable.contains_key(&id) {
      warn!("told to attempt signing a batch we aren't currently signing for");
      return;
    };

    // Delete any existing machines
    self.preprocessing.remove(&id);
    self.signing.remove(&id);

    // Update the attempt number
    self.attempt.insert(id, attempt);

    let id = SignId { key: self.keys.group_key().to_bytes().to_vec(), id, attempt };
    info!("signing batch {} #{}", hex::encode(id.id), id.attempt);

    // If we reboot mid-sign, the current design has us abort all signs and wait for latter
    // attempts/new signing protocols
    // This is distinct from the DKG which will continue DKG sessions, even on reboot
    // This is because signing is tolerant of failures of up to 1/3rd of the group
    // The DKG requires 100% participation
    // While we could apply similar tricks as the DKG (a seeded RNG) to achieve support for
    // reboots, it's not worth the complexity when messing up here leaks our secret share
    //
    // Despite this, on reboot, we'll get told of active signing items, and may be in this
    // branch again for something we've already attempted
    //
    // Only run if this hasn't already been attempted
    if self.db.has_attempt(&id) {
      warn!(
        "already attempted {} #{}. this is an error if we didn't reboot",
        hex::encode(id.id),
        id.attempt
      );
      return;
    }

    let mut txn = self.db.0.txn();
    SubstrateSignerDb::<D>::attempt(&mut txn, &id);
    txn.commit();

    // b"substrate" is a literal from sp-core
    let machine = AlgorithmMachine::new(Schnorrkel::new(b"substrate"), self.keys.clone());

    let (machine, preprocess) = machine.preprocess(&mut OsRng);
    self.preprocessing.insert(id.id, machine);

    // Broadcast our preprocess
    self.events.push_back(SubstrateSignerEvent::ProcessorMessage(
      ProcessorMessage::BatchPreprocess { id, preprocess: preprocess.serialize() },
    ));
  }

  pub async fn sign(&mut self, batch: Batch) {
    if self.db.completed(batch.block.0) {
      debug!("Sign batch order for ID we've already completed signing");
      // See batch_signed for commentary on why this simply returns
      return;
    }

    let id = batch.block.0;
    self.signable.insert(id, batch);
    self.attempt(id, 0).await;
  }

  pub async fn handle(&mut self, msg: CoordinatorMessage) {
    match msg {
      CoordinatorMessage::BatchPreprocesses { id, mut preprocesses } => {
        if self.verify_id(&id).is_err() {
          return;
        }

        let machine = match self.preprocessing.remove(&id.id) {
          // Either rebooted or RPC error, or some invariant
          None => {
            warn!(
              "not preprocessing for {}. this is an error if we didn't reboot",
              hex::encode(id.id)
            );
            return;
          }
          Some(machine) => machine,
        };

        let preprocesses = match preprocesses
          .drain()
          .map(|(l, preprocess)| {
            machine
              .read_preprocess::<&[u8]>(&mut preprocess.as_ref())
              .map(|preprocess| (l, preprocess))
          })
          .collect::<Result<_, _>>()
        {
          Ok(preprocesses) => preprocesses,
          Err(e) => todo!("malicious signer: {:?}", e),
        };

        let (machine, share) = match machine.sign(preprocesses, &self.signable[&id.id].encode()) {
          Ok(res) => res,
          Err(e) => todo!("malicious signer: {:?}", e),
        };
        self.signing.insert(id.id, machine);

        // Broadcast our share
        let mut share_bytes = [0; 32];
        share_bytes.copy_from_slice(&share.serialize());
        self.events.push_back(SubstrateSignerEvent::ProcessorMessage(
          ProcessorMessage::BatchShare { id, share: share_bytes },
        ));
      }

      CoordinatorMessage::BatchShares { id, mut shares } => {
        if self.verify_id(&id).is_err() {
          return;
        }

        let machine = match self.signing.remove(&id.id) {
          // Rebooted, RPC error, or some invariant
          None => {
            // If preprocessing has this ID, it means we were never sent the preprocess by the
            // coordinator
            if self.preprocessing.contains_key(&id.id) {
              panic!("never preprocessed yet signing?");
            }

            warn!(
              "not preprocessing for {}. this is an error if we didn't reboot",
              hex::encode(id.id)
            );
            return;
          }
          Some(machine) => machine,
        };

        let shares = match shares
          .drain()
          .map(|(l, share)| {
            machine.read_share::<&[u8]>(&mut share.as_ref()).map(|share| (l, share))
          })
          .collect::<Result<_, _>>()
        {
          Ok(shares) => shares,
          Err(e) => todo!("malicious signer: {:?}", e),
        };

        let sig = match machine.complete(shares) {
          Ok(res) => res,
          Err(e) => todo!("malicious signer: {:?}", e),
        };

        let batch =
          SignedBatch { batch: self.signable.remove(&id.id).unwrap(), signature: sig.into() };

        // Save the batch in case it's needed for recovery
        let mut txn = self.db.0.txn();
        SubstrateSignerDb::<D>::save_batch(&mut txn, &batch);
        SubstrateSignerDb::<D>::complete(&mut txn, id.id);
        txn.commit();

        // Stop trying to sign for this batch
        assert!(self.attempt.remove(&id.id).is_some());
        assert!(self.preprocessing.remove(&id.id).is_none());
        assert!(self.signing.remove(&id.id).is_none());

        self.events.push_back(SubstrateSignerEvent::SignedBatch(batch));
      }

      CoordinatorMessage::BatchReattempt { id } => {
        self.attempt(id.id, id.attempt).await;
      }
    }
  }

  pub fn batch_signed(&mut self, block: BlockHash) {
    // Stop trying to sign for this batch
    let mut txn = self.db.0.txn();
    SubstrateSignerDb::<D>::complete(&mut txn, block.0);
    txn.commit();

    self.signable.remove(&block.0);
    self.attempt.remove(&block.0);
    self.preprocessing.remove(&block.0);
    self.signing.remove(&block.0);

    // This doesn't emit SignedBatch because it doesn't have access to the SignedBatch
    // This function is expected to only be called once Substrate acknowledges this block,
    // which means its batch must have been signed
    // While a successive batch's signing would also cause this block to be acknowledged, Substrate
    // guarantees a batch's ordered inclusion

    // This also doesn't emit any further events since all mutation from the Batch being signed
    // happens on the substrate::CoordinatorMessage::SubstrateBlock message (which SignedBatch is
    // meant to end up triggering)
  }
}
