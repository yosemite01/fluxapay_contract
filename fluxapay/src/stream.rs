use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, Env, String, Symbol, Vec,
};

// ─── Data types ───────────────────────────────────────────────────────────────

/// Status of a payment stream.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StreamStatus {
    /// Stream is actively draining the deposit at `rate_per_second`.
    Active,
    /// Stream was cancelled by the sender; residual deposit already refunded.
    Cancelled,
    /// Deposit was fully drained; stream reached its natural end.
    Exhausted,
}

/// A continuous payment stream from `sender` to `receiver`.
///
/// Tokens flow at `rate_per_second` until either the deposit is exhausted or
/// the stream is cancelled. The sender may call [`decrease_rate_per_second`] at
/// any time to slow the flow; accrued amounts are check-pointed before the new
/// rate takes effect and any surplus deposit is refunded.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaymentStream {
    /// Unique identifier for this stream.
    pub stream_id: String,
    /// Address that funded the stream (payer).
    pub sender: Address,
    /// Address receiving the streamed tokens.
    pub receiver: Address,
    /// Token contract address (e.g. USDC).
    pub token: Address,
    /// Current flow rate in the smallest token unit per second.
    pub rate_per_second: i128,
    /// Total deposit locked in this contract on behalf of the stream.
    pub remaining_deposit: i128,
    /// Ledger timestamp of the last checkpoint.
    pub last_checkpoint_at: u64,
    /// Cumulative tokens accrued **up to** `last_checkpoint_at`.
    ///
    /// Accrual since the last checkpoint is calculated lazily:
    /// `total_accrued = accrued_at_checkpoint + (now - last_checkpoint_at) * rate_per_second`
    pub accrued_at_checkpoint: i128,
    /// Stream lifecycle state.
    pub status: StreamStatus,
}

/// Storage key for a [`PaymentStream`].
#[contracttype]
pub enum StreamDataKey {
    Stream(String),
}

// ─── Errors ───────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum StreamError {
    /// No stream exists with the given ID.
    StreamNotFound = 1,
    /// Caller is not the sender of the stream.
    Unauthorized = 2,
    /// The new rate must be strictly less than the current rate.
    RateNotDecreased = 3,
    /// Rate cannot be zero or negative.
    InvalidRate = 4,
    /// A stream with that ID already exists.
    StreamAlreadyExists = 5,
    /// Deposit must be positive.
    InvalidDeposit = 6,
    /// Stream is not active.
    StreamNotActive = 7,
}

// ─── Contract ─────────────────────────────────────────────────────────────────

#[contract]
pub struct PaymentStreaming;

#[contractimpl]
#[allow(deprecated)] // events::publish — migrate to #[contractevent] in a follow-up
impl PaymentStreaming {
    /// Contract version bump helper.
    pub fn version() -> u32 {
        1
    }

    // ─── Stream creation ──────────────────────────────────────────────────────

    /// Create a new payment stream.
    ///
    /// The caller (sender) transfers `deposit` tokens from their account into
    /// this contract. Streaming begins immediately at `rate_per_second`.
    ///
    /// # Parameters
    /// * `sender`         – Account funding the stream; must sign the transaction.
    /// * `receiver`       – Account that will receive streamed tokens.
    /// * `token`          – Token contract address.
    /// * `rate_per_second`– Tokens per second to stream.
    /// * `deposit`        – Total tokens deposited upfront.
    /// * `stream_id`      – Caller-supplied unique identifier.
    pub fn create_stream(
        env: Env,
        sender: Address,
        receiver: Address,
        token: Address,
        rate_per_second: i128,
        deposit: i128,
        stream_id: String,
    ) -> Result<PaymentStream, StreamError> {
        sender.require_auth();

        if rate_per_second <= 0 {
            return Err(StreamError::InvalidRate);
        }
        if deposit <= 0 {
            return Err(StreamError::InvalidDeposit);
        }
        if env
            .storage()
            .persistent()
            .has(&StreamDataKey::Stream(stream_id.clone()))
        {
            return Err(StreamError::StreamAlreadyExists);
        }

        let now = env.ledger().timestamp();
        let stream = PaymentStream {
            stream_id: stream_id.clone(),
            sender,
            receiver,
            token: token.clone(),
            rate_per_second,
            remaining_deposit: deposit,
            last_checkpoint_at: now,
            accrued_at_checkpoint: 0,
            status: StreamStatus::Active,
        };

        // Persist state before interaction (reentrancy protection)
        env.storage()
            .persistent()
            .set(&StreamDataKey::Stream(stream_id.clone()), &stream);

        // Interaction: Transfer deposit from sender into this contract.
        let token_client = token::Client::new(&env, &token);
        token_client.transfer(&stream.sender, &env.current_contract_address(), &deposit);

        env.events().publish(
            (
                Symbol::new(&env, "STREAM"),
                Symbol::new(&env, "CREATED"),
                stream_id,
            ),
            (stream.sender.clone(), stream.receiver.clone(), deposit),
        );

        Ok(stream)
    }

    // ─── Rate decrease ────────────────────────────────────────────────────────

    /// Reduce the flow rate of an active stream.
    ///
    /// This function:
    /// 1. **Checkpoints** the `accrued_amount` and current timestamp so that
    ///    earnings up to this moment are locked in before the rate changes.
    /// 2. **Refunds** any portion of the deposit that exceeds the tokens still
    ///    accruing at the new (lower) rate:
    ///    `surplus = remaining_deposit - accrued_since_checkpoint - new_needed`
    ///    where `new_needed` is what the new rate needs to sustain for the same
    ///    remaining seconds the old rate would have run.
    /// 3. **Emits** a `RateDecreased` event.
    ///
    /// # Parameters
    /// * `sender`       – Must be the original stream sender; must sign.
    /// * `stream_id`    – Stream to update.
    /// * `new_rate`     – New rate (must be strictly less than the current rate).
    pub fn decrease_rate_per_second(
        env: Env,
        sender: Address,
        stream_id: String,
        new_rate: i128,
    ) -> Result<(), StreamError> {
        sender.require_auth();

        // ── Load stream ──────────────────────────────────────────────────────
        let mut stream: PaymentStream = env
            .storage()
            .persistent()
            .get(&StreamDataKey::Stream(stream_id.clone()))
            .ok_or(StreamError::StreamNotFound)?;

        // ── Authorization ────────────────────────────────────────────────────
        if stream.sender != sender {
            return Err(StreamError::Unauthorized);
        }

        // ── Validation ───────────────────────────────────────────────────────
        if stream.status != StreamStatus::Active {
            return Err(StreamError::StreamNotActive);
        }
        if new_rate <= 0 {
            return Err(StreamError::InvalidRate);
        }
        if new_rate >= stream.rate_per_second {
            return Err(StreamError::RateNotDecreased);
        }

        let now = env.ledger().timestamp();
        let old_rate = stream.rate_per_second;

        // ── Step 1: Checkpoint accrued_amount ─────────────────────────────────
        // Calculate how many tokens accrued since the last checkpoint.
        let elapsed = now.saturating_sub(stream.last_checkpoint_at);
        let newly_accrued = (elapsed as i128).saturating_mul(old_rate);

        // Clamp so we never accrue more than the remaining deposit.
        let newly_accrued = newly_accrued.min(stream.remaining_deposit - stream.accrued_at_checkpoint);

        stream.accrued_at_checkpoint = stream
            .accrued_at_checkpoint
            .saturating_add(newly_accrued);
        stream.last_checkpoint_at = now;

        // ── Step 2: Calculate surplus and refund ──────────────────────────────
        // Tokens not yet transferred to the receiver that are no longer needed
        // given the lower rate are returned to the sender.
        //
        // remaining_unlocked = remaining_deposit − accrued_at_checkpoint
        //   (i.e. the portion of deposit not yet "earned" by receiver)
        //
        // With the old rate those unlocked tokens would last:
        //   old_seconds_left = remaining_unlocked / old_rate
        //
        // At the new (lower) rate those same seconds need fewer tokens:
        //   new_needed = old_seconds_left * new_rate
        //
        // surplus = remaining_unlocked − new_needed
        let remaining_unlocked = stream
            .remaining_deposit
            .saturating_sub(stream.accrued_at_checkpoint);

        let surplus = if remaining_unlocked > 0 && old_rate > 0 {
            // integer division — gives floor of seconds left at old rate
            let old_seconds_left = remaining_unlocked / old_rate;
            let new_needed = old_seconds_left.saturating_mul(new_rate);
            remaining_unlocked.saturating_sub(new_needed).max(0)
        } else {
            0
        };

        // Apply the new rate.
        stream.rate_per_second = new_rate;

        // Persist state before interaction (reentrancy protection)
        env.storage()
            .persistent()
            .set(&StreamDataKey::Stream(stream_id.clone()), &stream);

        // Interaction: Transfer surplus back to sender.
        if surplus > 0 {
            let token_client = token::Client::new(&env, &stream.token);
            token_client.transfer(&env.current_contract_address(), &stream.sender, &surplus);
        }

        // ── Step 3: Emit RateDecreased event ─────────────────────────────────
        env.events().publish(
            (
                Symbol::new(&env, "STREAM"),
                Symbol::new(&env, "RATE_DECREASED"),
                stream_id,
            ),
            (sender, old_rate, new_rate, surplus),
        );

        Ok(())
    }

    // ─── Read helpers ─────────────────────────────────────────────────────────

    /// Return the stored stream, or `StreamNotFound`.
    pub fn get_stream(env: Env, stream_id: String) -> Result<PaymentStream, StreamError> {
        env.storage()
            .persistent()
            .get(&StreamDataKey::Stream(stream_id))
            .ok_or(StreamError::StreamNotFound)
    }

    /// Compute the total tokens accrued at the current ledger timestamp.
    ///
    /// This is a **view-only** helper and does not modify state.
    pub fn get_accrued_amount(env: Env, stream_id: String) -> Result<i128, StreamError> {
        let stream: PaymentStream = env
            .storage()
            .persistent()
            .get(&StreamDataKey::Stream(stream_id))
            .ok_or(StreamError::StreamNotFound)?;

        if stream.status != StreamStatus::Active {
            return Ok(stream.accrued_at_checkpoint);
        }

        let now = env.ledger().timestamp();
        let elapsed = now.saturating_sub(stream.last_checkpoint_at);
        let newly_accrued = (elapsed as i128).saturating_mul(stream.rate_per_second);
        let total = stream
            .accrued_at_checkpoint
            .saturating_add(newly_accrued)
            .min(stream.remaining_deposit);

        Ok(total)
    }

    /// Top up multiple streams in a single atomic transaction.
    ///
    /// The caller (sender) must be the sender of ALL specified streams.
    ///
    /// # Parameters
    /// * `sender`     – Must be the sender of all streams; must sign.
    /// * `top_ups`    – Vector of (stream_id, amount) tuples.
    pub fn top_up_multiple_streams(
        env: Env,
        sender: Address,
        top_ups: Vec<(String, i128)>,
    ) -> Result<(), StreamError> {
        sender.require_auth();

        for top_up in top_ups.iter() {
            let (stream_id, amount) = top_up;
            if amount <= 0 {
                return Err(StreamError::InvalidDeposit);
            }

            let mut stream: PaymentStream = env
                .storage()
                .persistent()
                .get(&StreamDataKey::Stream(stream_id.clone()))
                .ok_or(StreamError::StreamNotFound)?;

            if stream.sender != sender {
                return Err(StreamError::Unauthorized);
            }
            if stream.status != StreamStatus::Active {
                return Err(StreamError::StreamNotActive);
            }

            // Effects
            stream.remaining_deposit = stream.remaining_deposit.saturating_add(amount);
            
            // Persist state before interaction
            env.storage()
                .persistent()
                .set(&StreamDataKey::Stream(stream_id.clone()), &stream);

            // Interaction
            let token_client = token::Client::new(&env, &stream.token);
            token_client.transfer(&sender, &env.current_contract_address(), &amount);

            // Event
            env.events().publish(
                (
                    Symbol::new(&env, "STREAM"),
                    Symbol::new(&env, "TOPPED_UP"),
                    stream_id,
                ),
                (sender.clone(), amount),
            );
        }

        Ok(())
    }

    // ─── Stream cancellation ──────────────────────────────────────────────────

    /// Cancel an active stream and refund any remaining un-accrued deposit to
    /// the sender.
    ///
    /// # Parameters
    /// * `sender`    – Must be the original stream sender; must sign.
    /// * `stream_id` – Stream to cancel.
    pub fn cancel_stream(
        env: Env,
        sender: Address,
        stream_id: String,
    ) -> Result<(), StreamError> {
        sender.require_auth();

        let mut stream: PaymentStream = env
            .storage()
            .persistent()
            .get(&StreamDataKey::Stream(stream_id.clone()))
            .ok_or(StreamError::StreamNotFound)?;

        if stream.sender != sender {
            return Err(StreamError::Unauthorized);
        }
        if stream.status != StreamStatus::Active {
            return Err(StreamError::StreamNotActive);
        }

        let now = env.ledger().timestamp();

        // Checkpoint accrued amount up to now
        let elapsed = now.saturating_sub(stream.last_checkpoint_at);
        let newly_accrued = (elapsed as i128)
            .saturating_mul(stream.rate_per_second)
            .min(stream.remaining_deposit - stream.accrued_at_checkpoint);
        stream.accrued_at_checkpoint = stream.accrued_at_checkpoint.saturating_add(newly_accrued);
        stream.last_checkpoint_at = now;

        let accrued = stream.accrued_at_checkpoint;
        let refund = stream.remaining_deposit.saturating_sub(accrued);

        // Effects: mark cancelled
        stream.status = StreamStatus::Cancelled;
        stream.remaining_deposit = accrued; // only accrued portion remains

        // Persist before interaction (CEI pattern)
        env.storage()
            .persistent()
            .set(&StreamDataKey::Stream(stream_id.clone()), &stream);

        // Interaction: send accrued amount to receiver, refund to sender
        let token_client = token::Client::new(&env, &stream.token);
        if accrued > 0 {
            token_client.transfer(
                &env.current_contract_address(),
                &stream.receiver,
                &accrued,
            );
        }
        if refund > 0 {
            token_client.transfer(&env.current_contract_address(), &stream.sender, &refund);
        }

        env.events().publish(
            (
                Symbol::new(&env, "STREAM"),
                Symbol::new(&env, "CANCELLED"),
                stream_id,
            ),
            (sender, accrued, refund),
        );

        Ok(())
    }

    /// Cancel multiple streams atomically.
    ///
    /// Each stream is cancelled in order; if any stream fails the whole call
    /// reverts.  Returns the list of cancelled stream IDs.
    ///
    /// # Parameters
    /// * `sender`     – Must be the sender of all streams; must sign.
    /// * `stream_ids` – IDs of streams to cancel.
    pub fn cancel_multiple_streams(
        env: Env,
        sender: Address,
        stream_ids: Vec<String>,
    ) -> Result<Vec<String>, StreamError> {
        sender.require_auth();

        let mut cancelled = Vec::new(&env);
        for stream_id in stream_ids.iter() {
            let mut stream: PaymentStream = env
                .storage()
                .persistent()
                .get(&StreamDataKey::Stream(stream_id.clone()))
                .ok_or(StreamError::StreamNotFound)?;

            if stream.sender != sender {
                return Err(StreamError::Unauthorized);
            }
            if stream.status != StreamStatus::Active {
                return Err(StreamError::StreamNotActive);
            }

            let now = env.ledger().timestamp();
            let elapsed = now.saturating_sub(stream.last_checkpoint_at);
            let newly_accrued = (elapsed as i128)
                .saturating_mul(stream.rate_per_second)
                .min(stream.remaining_deposit - stream.accrued_at_checkpoint);
            stream.accrued_at_checkpoint =
                stream.accrued_at_checkpoint.saturating_add(newly_accrued);
            stream.last_checkpoint_at = now;

            let accrued = stream.accrued_at_checkpoint;
            let refund = stream.remaining_deposit.saturating_sub(accrued);

            // Effects
            stream.status = StreamStatus::Cancelled;
            stream.remaining_deposit = accrued;
            env.storage()
                .persistent()
                .set(&StreamDataKey::Stream(stream_id.clone()), &stream);

            // Interactions
            let token_client = token::Client::new(&env, &stream.token);
            if accrued > 0 {
                token_client.transfer(
                    &env.current_contract_address(),
                    &stream.receiver,
                    &accrued,
                );
            }
            if refund > 0 {
                token_client.transfer(&env.current_contract_address(), &stream.sender, &refund);
            }

            env.events().publish(
                (
                    Symbol::new(&env, "STREAM"),
                    Symbol::new(&env, "CANCELLED"),
                    stream_id.clone(),
                ),
                (sender.clone(), accrued, refund),
            );

            cancelled.push_back(stream_id);
        }

        Ok(cancelled)
    }

    // ─── Batch withdrawal ─────────────────────────────────────────────────────

    /// Withdraw accrued amounts from multiple streams to specified destinations
    /// in a single transaction.
    ///
    /// The caller must be the receiver of each stream.  Returns the list of
    /// stream IDs that were processed.
    ///
    /// # Parameters
    /// * `recipient`   – Must be the receiver of all streams; must sign.
    /// * `withdrawals` – Vector of `WithdrawalRecipient` (stream_id, destination, amount).
    pub fn batch_withdraw_to(
        env: Env,
        recipient: Address,
        withdrawals: Vec<crate::WithdrawalRecipient>,
    ) -> Result<Vec<String>, StreamError> {
        recipient.require_auth();

        let mut processed = Vec::new(&env);
        for w in withdrawals.iter() {
            let mut stream: PaymentStream = env
                .storage()
                .persistent()
                .get(&StreamDataKey::Stream(w.stream_id.clone()))
                .ok_or(StreamError::StreamNotFound)?;

            if stream.receiver != recipient {
                return Err(StreamError::Unauthorized);
            }
            if stream.status != StreamStatus::Active {
                return Err(StreamError::StreamNotActive);
            }

            // Recompute accrued up to now
            let now = env.ledger().timestamp();
            let elapsed = now.saturating_sub(stream.last_checkpoint_at);
            let newly_accrued = (elapsed as i128)
                .saturating_mul(stream.rate_per_second)
                .min(stream.remaining_deposit - stream.accrued_at_checkpoint);
            stream.accrued_at_checkpoint =
                stream.accrued_at_checkpoint.saturating_add(newly_accrued);
            stream.last_checkpoint_at = now;

            let withdrawable = stream.accrued_at_checkpoint.min(w.amount).max(0);
            if withdrawable == 0 {
                continue;
            }

            // Effects
            stream.accrued_at_checkpoint =
                stream.accrued_at_checkpoint.saturating_sub(withdrawable);
            stream.remaining_deposit = stream.remaining_deposit.saturating_sub(withdrawable);

            if stream.remaining_deposit == 0 {
                stream.status = StreamStatus::Exhausted;
            }

            env.storage()
                .persistent()
                .set(&StreamDataKey::Stream(w.stream_id.clone()), &stream);

            // Interaction
            let token_client = token::Client::new(&env, &stream.token);
            token_client.transfer(
                &env.current_contract_address(),
                &w.destination,
                &withdrawable,
            );

            env.events().publish(
                (
                    Symbol::new(&env, "STREAM"),
                    Symbol::new(&env, "WITHDRAWN"),
                    w.stream_id.clone(),
                ),
                (recipient.clone(), w.destination.clone(), withdrawable),
            );

            processed.push_back(w.stream_id.clone());
        }

        Ok(processed)
    }
}
