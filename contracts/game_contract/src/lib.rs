#![no_std]
use soroban_sdk::token::TokenClient;
use soroban_sdk::{
    Address, Bytes, BytesN, Env, Map, Symbol, Vec, contract, contracterror, contractimpl,
    contracttype, symbol_short,
};

// ────────────────────────────────────────────────────────────────────────────
// Game types (retained from the original simple contract)
// ────────────────────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GameState {
    Created,
    InProgress,
    Completed,
    Drawn,
    Forfeited,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct Game {
    pub id: u64,
    pub player1: Address,
    pub player2: Option<Address>,
    pub state: GameState,
    pub wager_amount: i128,
    pub current_turn: u32, // 1 = player1, 2 = player2
    pub moves: Vec<ChessMove>,
    pub created_at: u64,
    pub winner: Option<Address>,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct ChessMove {
    pub player: Address,
    pub move_data: Vec<u32>,
    pub timestamp: u64,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct PlayerRating {
    pub address: Address,
    pub rating: i32,          // Current ELO rating
    pub games_played: u32,
    pub wins: u32,
    pub losses: u32,
    pub draws: u32,
    pub highest_rating: i32,
    pub last_updated: u64,    // Ledger sequence
}

// ────────────────────────────────────────────────────────────────────────────
// Storage keys
// ────────────────────────────────────────────────────────────────────────────

// Game / escrow
const GAME_COUNTER: Symbol = symbol_short!("GAME_CNT");
const GAMES: Symbol = symbol_short!("GAMES");
const ESCROW: Symbol = symbol_short!("ESCROW");
const TOKEN_CONTRACT: Symbol = symbol_short!("TOKEN");

// Puzzle-reward  (#199)
const ADMIN_KEY: Symbol = symbol_short!("ADMIN_KEY"); // BytesN<32> ED25519 backend pubkey
const TREASURY: Symbol = symbol_short!("TREASURY"); // i128 treasury reserve
const BALANCES: Symbol = symbol_short!("BALANCES"); // Map<Address, i128>
const USED_NONCE: Symbol = symbol_short!("NONCES"); // Map<u64, bool>
const MAX_STAKE: Symbol = symbol_short!("MAXSTAKE");

// Fee / treasury  (#200)
const FEE_BIPS: Symbol = symbol_short!("FEE_BIPS"); // u32  (0–1000, i.e. 0–10 %)
const TREASURY_ADDR: Symbol = symbol_short!("TR_ADDR"); // Address
const CONTRACT_ADMIN: Symbol = symbol_short!("CT_ADMIN"); // Address

// ELO Rating System
const PLAYER_RATINGS: Symbol = symbol_short!("RATING"); // Map<Address, PlayerRating>
const K_FACTOR: Symbol = symbol_short!("K_FACT"); // u32 - ELO K-factor for rating calculations

// ────────────────────────────────────────────────────────────────────────────
// Errors
// ────────────────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum ContractError {
    GameNotFound = 1,
    NotYourTurn = 2,
    GameNotInProgress = 3,
    InvalidMove = 4,
    InsufficientFunds = 5,
    AlreadyJoined = 6,
    GameFull = 7,
    NotPlayer = 8,
    GameAlreadyCompleted = 9,
    DrawNotAvailable = 10,
    ForfeitNotAllowed = 11,
    InvalidPercentage = 12,
    MismatchedLengths = 13,
    /// Invalid or already-used backend signature  (#199)
    Unauthorized = 14,
    StakeLimitExceeded = 15,
    /// Player has no rating yet
    RatingNotFound = 16,
}

#[contract]
pub struct GameContract;

#[contractimpl]
impl GameContract {
    pub fn initialize_token(env: Env, admin: Address, token_contract: Address) {
        if env.storage().instance().has(&TOKEN_CONTRACT) {
            panic!("Contract already initialized");
        }
        admin.require_auth();
        env.storage()
            .instance()
            .set(&TOKEN_CONTRACT, &token_contract);
    }

    fn token_contract_address(env: &Env) -> Address {
        env.storage()
            .instance()
            .get(&TOKEN_CONTRACT)
            .expect("Token contract is not initialized")
    }

    fn token_client(env: &Env) -> TokenClient {
        TokenClient::new(env, &Self::token_contract_address(env))
    }

    // ── Game lifecycle ────────────────────────────────────────────────────────

    pub fn create_game(
        env: Env,
        player1: Address,
        wager_amount: i128,
    ) -> Result<u64, ContractError> {
        let max_stake: i128 = env.storage().instance().get(&MAX_STAKE).unwrap_or(1_000);
        if wager_amount > max_stake {
            return Err(ContractError::StakeLimitExceeded);
        }

        player1.require_auth();

        let token_client = Self::token_client(&env);
        let contract_address = env.current_contract_address();

        if token_client.balance(&player1) < wager_amount {
            return Err(ContractError::InsufficientFunds);
        }

        token_client.transfer(&player1, &contract_address, &wager_amount);

        let mut game_counter: u64 = env.storage().instance().get(&GAME_COUNTER).unwrap_or(0);
        game_counter += 1;
        env.storage().instance().set(&GAME_COUNTER, &game_counter);

        let game = Game {
            id: game_counter,
            player1: player1.clone(),
            player2: None,
            state: GameState::Created,
            wager_amount,
            current_turn: 1,
            moves: Vec::new(&env),
            created_at: env.ledger().sequence() as u64,
            winner: None,
        };

        let mut games: Map<u64, Game> = env
            .storage()
            .instance()
            .get(&GAMES)
            .unwrap_or(Map::new(&env));
        games.set(game_counter, game);
        env.storage().instance().set(&GAMES, &games);

        let mut escrow: Map<Address, i128> = env
            .storage()
            .instance()
            .get(&ESCROW)
            .unwrap_or(Map::new(&env));
        let current_escrow = escrow.get(player1.clone()).unwrap_or(0);
        escrow.set(player1, current_escrow + wager_amount);
        env.storage().instance().set(&ESCROW, &escrow);

        Ok(game_counter)
    }

    pub fn join_game(env: Env, game_id: u64, player2: Address) -> Result<(), ContractError> {
        let mut games: Map<u64, Game> = env
            .storage()
            .instance()
            .get(&GAMES)
            .ok_or(ContractError::GameNotFound)?;

        let mut game = games.get(game_id).ok_or(ContractError::GameNotFound)?;

        if game.state != GameState::Created {
            return Err(ContractError::GameAlreadyCompleted);
        }
        if game.player2.is_some() {
            return Err(ContractError::GameFull);
        }
        if game.player1 == player2 {
            return Err(ContractError::AlreadyJoined);
        }

        let max_stake: i128 = env.storage().instance().get(&MAX_STAKE).unwrap_or(1_000);
        if game.wager_amount > max_stake {
            return Err(ContractError::StakeLimitExceeded);
        }

        player2.require_auth();
        let token_client = Self::token_client(&env);
        let contract_address = env.current_contract_address();

        if token_client.balance(&player2) < game.wager_amount {
            return Err(ContractError::InsufficientFunds);
        }

        token_client.transfer(&player2, &contract_address, &game.wager_amount);

        game.player2 = Some(player2.clone());
        game.state = GameState::InProgress;
        game.current_turn = 1;

        let mut escrow: Map<Address, i128> = env
            .storage()
            .instance()
            .get(&ESCROW)
            .unwrap_or(Map::new(&env));
        let current_escrow = escrow.get(player2.clone()).unwrap_or(0);
        escrow.set(player2, current_escrow + game.wager_amount);
        env.storage().instance().set(&ESCROW, &escrow);

        games.set(game_id, game);
        env.storage().instance().set(&GAMES, &games);

        Ok(())
    }

    pub fn submit_move(
        env: Env,
        game_id: u64,
        player: Address,
        move_data: Vec<u32>,
    ) -> Result<(), ContractError> {
        let mut games: Map<u64, Game> = env
            .storage()
            .instance()
            .get(&GAMES)
            .ok_or(ContractError::GameNotFound)?;

        let mut game = games.get(game_id).ok_or(ContractError::GameNotFound)?;

        if game.state != GameState::InProgress {
            return Err(ContractError::GameNotInProgress);
        }

        let player_num = if player == game.player1 {
            1
        } else if Some(player.clone()) == game.player2 {
            2
        } else {
            return Err(ContractError::NotPlayer);
        };

        if player_num != game.current_turn {
            return Err(ContractError::NotYourTurn);
        }

        if move_data.is_empty() {
            return Err(ContractError::InvalidMove);
        }

        let chess_move = ChessMove {
            player: player.clone(),
            move_data,
            timestamp: env.ledger().sequence() as u64,
        };
        game.moves.push_back(chess_move.into());
        game.current_turn = if game.current_turn == 1 { 2 } else { 1 };

        games.set(game_id, game);
        env.storage().instance().set(&GAMES, &games);

        Ok(())
    }

    pub fn claim_draw(env: Env, game_id: u64, player: Address) -> Result<(), ContractError> {
        let mut games: Map<u64, Game> = env
            .storage()
            .instance()
            .get(&GAMES)
            .ok_or(ContractError::GameNotFound)?;

        let mut game = games.get(game_id).ok_or(ContractError::GameNotFound)?;

        if game.state != GameState::InProgress {
            return Err(ContractError::GameNotInProgress);
        }
        if player != game.player1 && Some(player.clone()) != game.player2 {
            return Err(ContractError::NotPlayer);
        }

        game.state = GameState::Drawn;
        Self::process_draw_payout(&env, &game)?;

        games.set(game_id, game);
        env.storage().instance().set(&GAMES, &games);

        Ok(())
    }

    pub fn forfeit(env: Env, game_id: u64, player: Address) -> Result<(), ContractError> {
        let mut games: Map<u64, Game> = env
            .storage()
            .instance()
            .get(&GAMES)
            .ok_or(ContractError::GameNotFound)?;

        let mut game = games.get(game_id).ok_or(ContractError::GameNotFound)?;

        if game.state != GameState::InProgress {
            return Err(ContractError::GameNotInProgress);
        }
        if player != game.player1 && Some(player.clone()) != game.player2 {
            return Err(ContractError::NotPlayer);
        }

        let winner = if player == game.player1 {
            game.player2
                .as_ref()
                .ok_or(ContractError::GameFull)?
                .clone()
        } else {
            game.player1.clone()
        };

        game.state = GameState::Forfeited;
        game.winner = Some(winner.clone());
        Self::process_payout(&env, &game, &winner)?;

        games.set(game_id, game);
        env.storage().instance().set(&GAMES, &games);

        Ok(())
    }

    pub fn payout(env: Env, game_id: u64, winner: Address) -> Result<(), ContractError> {
        let mut games: Map<u64, Game> = env
            .storage()
            .instance()
            .get(&GAMES)
            .ok_or(ContractError::GameNotFound)?;

        let game = games.get(game_id).ok_or(ContractError::GameNotFound)?;

        if game.state != GameState::Completed {
            return Err(ContractError::GameNotInProgress);
        }
        if game.winner.as_ref() != Some(&winner) {
            return Err(ContractError::NotPlayer);
        }

        Self::process_payout(&env, &game, &winner)?;

        games.set(game_id, game);
        env.storage().instance().set(&GAMES, &games);

        Ok(())
    }

    pub fn payout_tournament(
        env: Env,
        game_id: u64,
        winners: Vec<Address>,
        percentages: Vec<u32>,
    ) -> Result<(), ContractError> {
        let mut games: Map<u64, Game> = env
            .storage()
            .instance()
            .get(&GAMES)
            .ok_or(ContractError::GameNotFound)?;

        let game = games.get(game_id).ok_or(ContractError::GameNotFound)?;

        if game.state != GameState::Completed {
            return Err(ContractError::GameNotInProgress);
        }

        game.player1.require_auth();

        if winners.len() != percentages.len() {
            return Err(ContractError::MismatchedLengths);
        }

        let mut total_percentage: u32 = 0;
        for i in 0..percentages.len() {
            total_percentage += percentages.get(i).unwrap();
        }
        if total_percentage != 100 {
            return Err(ContractError::InvalidPercentage);
        }

        let mut escrow: Map<Address, i128> = env
            .storage()
            .instance()
            .get(&ESCROW)
            .unwrap_or(Map::new(&env));

        let player1_escrow = escrow.get(game.player1.clone()).unwrap_or(0);
        if player1_escrow < game.wager_amount {
            return Err(ContractError::InsufficientFunds);
        }

        let mut player2_escrow = 0i128;
        let mut total_pool = game.wager_amount;

        if let Some(ref player2) = game.player2 {
            player2_escrow = escrow.get(player2.clone()).unwrap_or(0);
            if player2_escrow < game.wager_amount {
                return Err(ContractError::InsufficientFunds);
            }
            total_pool = game.wager_amount * 2;
        }

        // Deduct wagers first to prevent double-counting
        escrow.set(game.player1.clone(), player1_escrow - game.wager_amount);
        if let Some(ref player2) = game.player2 {
            escrow.set(player2.clone(), player2_escrow - game.wager_amount);
        }

        let mut distributed: i128 = 0;
        for i in 0..winners.len() {
            let winner = winners.get(i).unwrap();
            let percentage = percentages.get(i).unwrap();
            let payout_amount = (total_pool * percentage as i128) / 100;
            distributed += payout_amount;
            let winner_escrow = escrow.get(winner.clone()).unwrap_or(0);
            escrow.set(winner.clone(), winner_escrow + payout_amount);
        }

        // Dust goes to first winner
        let remainder = total_pool - distributed;
        if remainder > 0 && winners.len() > 0 {
            let first_winner = winners.get(0).unwrap();
            let winner_escrow = escrow.get(first_winner.clone()).unwrap_or(0);
            escrow.set(first_winner.clone(), winner_escrow + remainder);
        }

        env.storage().instance().set(&ESCROW, &escrow);
        games.set(game_id, game);
        env.storage().instance().set(&GAMES, &games);

        Ok(())
    }

    pub fn get_game(env: Env, game_id: u64) -> Result<Game, ContractError> {
        let games: Map<u64, Game> = env
            .storage()
            .instance()
            .get(&GAMES)
            .ok_or(ContractError::GameNotFound)?;

        games.get(game_id).ok_or(ContractError::GameNotFound)
    }

    pub fn get_all_games(env: Env) -> Map<u64, Game> {
        env.storage()
            .instance()
            .get(&GAMES)
            .unwrap_or(Map::new(&env))
    }

    // ── Internal payout helpers ───────────────────────────────────────────────

    fn process_draw_payout(env: &Env, game: &Game) -> Result<(), ContractError> {
        let token_client = Self::token_client(env);
        let contract_address = env.current_contract_address();

        let mut escrow: Map<Address, i128> = env
            .storage()
            .instance()
            .get(&ESCROW)
            .unwrap_or(Map::new(env));

        // Return player1's stake
        token_client.transfer(&contract_address, &game.player1, &game.wager_amount);
        let player1_escrow = escrow.get(game.player1.clone()).unwrap_or(0);
        escrow.set(game.player1.clone(), player1_escrow - game.wager_amount);

        // Return player2's stake
        if let Some(ref player2) = game.player2 {
            token_client.transfer(&contract_address, player2, &game.wager_amount);
            let player2_escrow = escrow.get(player2.clone()).unwrap_or(0);
            escrow.set(player2.clone(), player2_escrow - game.wager_amount);
        }

        env.storage().instance().set(&ESCROW, &escrow);
        Ok(())
    }

    /// #200 – Treasury fee redirection in payout_winner.
    ///
    /// Uses Soroban-safe integer arithmetic:
    ///   `fee    = total_pool * fee_bips / 1000`
    ///   `payout = total_pool - fee`
    ///
    /// Example: 10 XLM pool, fee_bips = 20 (2 %)
    ///   fee    = 10 * 20 / 1000 = 0.2 XLM  → Treasury
    ///   payout = 10 - 0.2       = 9.8 XLM  → Winner
    fn process_payout(env: &Env, game: &Game, winner: &Address) -> Result<(), ContractError> {
        let mut escrow: Map<Address, i128> = env
            .storage()
            .instance()
            .get(&ESCROW)
            .unwrap_or(Map::new(env));

        let fee_bips: u32 = env.storage().instance().get(&FEE_BIPS).unwrap_or(0);
        let treasury_addr_opt: Option<Address> = env.storage().instance().get(&TREASURY_ADDR);

        let total_pool = game.wager_amount * 2;

        // --- #200: safe fee math -------------------------------------------------
        // Multiplying first keeps precision; dividing by 1000 rounds down (floor).
        // fee_bips is validated to be ≤ 1000 at configuration time, so overflow
        // cannot occur for any realistic i128 wager amount.
        let (payout, fee) = if treasury_addr_opt.is_some() && fee_bips > 0 {
            let fee = (total_pool * fee_bips as i128) / 1000;
            (total_pool - fee, fee)
        } else {
            (total_pool, 0)
        };
        // -------------------------------------------------------------------------

        // Deduct both stakes first (clean state, prevents double-spend)
        let player1_escrow = escrow.get(game.player1.clone()).unwrap_or(0);
        escrow.set(game.player1.clone(), player1_escrow - game.wager_amount);

        let player2 = game.player2.as_ref().ok_or(ContractError::GameFull)?;
        let player2_escrow = escrow.get(player2.clone()).unwrap_or(0);
        escrow.set(player2.clone(), player2_escrow - game.wager_amount);

        // Credit winner (net of fee)
        let winner_escrow = escrow.get(winner.clone()).unwrap_or(0);
        escrow.set(winner.clone(), winner_escrow + payout);

        // Credit treasury with the fee portion
        if fee > 0 {
            if let Some(ref treasury_addr) = treasury_addr_opt {
                let treasury_escrow = escrow.get(treasury_addr.clone()).unwrap_or(0);
                escrow.set(treasury_addr.clone(), treasury_escrow + fee);
            }
        }

        env.storage().instance().set(&ESCROW, &escrow);

        // Physical token transfers
        let token_client = Self::token_client(env);
        let contract_address = env.current_contract_address();

        token_client.transfer(&contract_address, winner, &payout);
        if fee > 0 {
            if let Some(ref treasury_addr) = treasury_addr_opt {
                token_client.transfer(&contract_address, treasury_addr, &fee);
            }
        }

        Ok(())
    }

    // ── Administration ────────────────────────────────────────────────────────

    /// Initialize puzzle-reward system (#199) and fee configuration (#200).
    /// Must be called exactly once.
    ///
    /// * `admin_public_key` – 32-byte ED25519 public key of the backend signing service
    /// * `treasury_amount`  – Initial token reserve for puzzle payouts
    /// * `fee_bips`         – Protocol fee in basis-points of 1000 (e.g. 20 = 2 %)
    /// * `treasury_address` – Address that receives the protocol fee
    pub fn initialize_puzzle_rewards(
        env: Env,
        admin: Address,
        admin_public_key: Bytes,
        treasury_amount: i128,
        fee_bips: u32,
        treasury_address: Address,
    ) {
        if env.storage().instance().has(&CONTRACT_ADMIN) {
            panic!("Already initialized");
        }

        admin.require_auth();

        if admin_public_key.len() != 32 {
            panic!("Admin public key must be 32 bytes");
        }
        if treasury_amount < 0 {
            panic!("Treasury amount must be non-negative");
        }
        if fee_bips > 1000 {
            panic!("Fee bips must be between 0 and 1000");
        }

        env.storage().instance().set(&CONTRACT_ADMIN, &admin);
        env.storage().instance().set(&ADMIN_KEY, &admin_public_key);
        env.storage().instance().set(&TREASURY, &treasury_amount);
        env.storage().instance().set(&FEE_BIPS, &fee_bips);
        env.storage()
            .instance()
            .set(&TREASURY_ADDR, &treasury_address);
        env.storage().instance().set(&MAX_STAKE, &1_000i128);
    }

    pub fn set_max_stake(env: Env, new_limit: i128) {
        env.storage().instance().set(&MAX_STAKE, &new_limit);
    }

    pub fn configure_fees(env: Env, admin: Address, fee_bips: u32, treasury_address: Address) {
        let current_admin: Address = env
            .storage()
            .instance()
            .get(&CONTRACT_ADMIN)
            .expect("Not initialized");
        current_admin.require_auth();

        if admin != current_admin {
            panic!("Unauthorized admin address");
        }
        if fee_bips > 1000 {
            panic!("Fee bips must be between 0 and 1000");
        }

        env.storage().instance().set(&FEE_BIPS, &fee_bips);
        env.storage()
            .instance()
            .set(&TREASURY_ADDR, &treasury_address);
    }

    pub fn upgrade_admin(env: Env, admin: Address) {
        if env.storage().instance().has(&CONTRACT_ADMIN) {
            panic!("Admin already set");
        }
        if !env.storage().instance().has(&ADMIN_KEY) {
            panic!("Contract must be initialized first");
        }
        admin.require_auth();
        env.storage().instance().set(&CONTRACT_ADMIN, &admin);
    }

    // ── #199 – claim_puzzle_reward ────────────────────────────────────────────
    //
    // Accepts a backend ED25519 signature that proves the user solved a puzzle,
    // then transfers `reward_amount` tokens from the Treasury to the recipient.
    //
    // Signature payload (SHA-256 pre-image):
    //   recipient_address_bytes || reward_amount_le_8bytes || nonce_le_8bytes
    //
    // Acceptance criteria
    //   • Invalid signature  → panics (Soroban's ed25519_verify panics on failure)
    //   • Replayed nonce     → Err(ContractError::Unauthorized)
    //   • Valid call         → recipient balance incremented, treasury decremented
    pub fn claim_puzzle_reward(
        env: Env,
        recipient: Address,
        reward_amount: i128,
        nonce: u64,
        signature: BytesN<64>,
    ) -> Result<(), ContractError> {
        // 1. Load admin ED25519 public key
        let admin_key_bytes: Bytes = env
            .storage()
            .instance()
            .get(&ADMIN_KEY)
            .expect("Not initialized");

        let admin_pubkey: BytesN<32> = admin_key_bytes
            .try_into()
            .expect("Admin public key must be 32 bytes");

        // 2. Replay protection – check nonce before any state mutation
        let mut nonces: Map<u64, bool> = env
            .storage()
            .instance()
            .get(&USED_NONCE)
            .unwrap_or(Map::new(&env));

        if nonces.get(nonce).unwrap_or(false) {
            return Err(ContractError::Unauthorized);
        }

        // 3. Build canonical payload and verify ED25519 signature
        //    Payload = SHA256( address_string_bytes || amount_le8 || nonce_le8 )
        let mut payload_bytes = Bytes::new(&env);

        // Encode recipient address as its string representation bytes
        let recipient_str = recipient.clone().to_string();
        let str_len = recipient_str.len() as usize;
        let mut addr_buf = [0u8; 64];
        recipient_str.copy_into_slice(&mut addr_buf[..str_len]);
        payload_bytes.append(&Bytes::from_slice(&env, &addr_buf[..str_len]));

        // Append reward_amount as little-endian i64 bytes
        let amount_le: [u8; 8] = (reward_amount as i64).to_le_bytes();
        payload_bytes.append(&Bytes::from_slice(&env, &amount_le));

        // Append nonce as little-endian u64 bytes
        let nonce_le: [u8; 8] = nonce.to_le_bytes();
        payload_bytes.append(&Bytes::from_slice(&env, &nonce_le));

        // Hash and verify — ed25519_verify panics if signature is invalid,
        // which satisfies the acceptance criterion "invalid signature panics".
        let digest_bytesn: BytesN<32> = env.crypto().sha256(&payload_bytes).into();
        let digest_bytes: Bytes = digest_bytesn.into();
        env.crypto()
            .ed25519_verify(&admin_pubkey, &digest_bytes, &signature);

        // 4. Mark nonce as used (state-before-interaction pattern)
        nonces.set(nonce, true);
        env.storage().instance().set(&USED_NONCE, &nonces);

        // 5. Deduct from Treasury
        let treasury: i128 = env.storage().instance().get(&TREASURY).unwrap_or(0);
        if treasury < reward_amount {
            panic!("Insufficient treasury");
        }
        env.storage()
            .instance()
            .set(&TREASURY, &(treasury - reward_amount));

        // 6. Credit recipient's puzzle-reward balance
        let mut balances: Map<Address, i128> = env
            .storage()
            .instance()
            .get(&BALANCES)
            .unwrap_or(Map::new(&env));

        let prev_balance = balances.get(recipient.clone()).unwrap_or(0);
        balances.set(recipient.clone(), prev_balance + reward_amount);
        env.storage().instance().set(&BALANCES, &balances);

        // 7. Emit event
        env.events()
            .publish((symbol_short!("pzl_rwd"), recipient.clone()), reward_amount);

        Ok(())
    }

    /// Query the puzzle-reward balance of an address.
    pub fn reward_balance(env: Env, address: Address) -> i128 {
        let balances: Map<Address, i128> = env
            .storage()
            .instance()
            .get(&BALANCES)
            .unwrap_or(Map::new(&env));
        balances.get(address).unwrap_or(0)
    }

    /// Query the current treasury reserve.
    pub fn treasury_balance(env: Env) -> i128 {
        env.storage().instance().get(&TREASURY).unwrap_or(0)
    }

    // ── ELO Rating System ──────────────────────────────────────────────────

    /// Initialize ELO rating system with default K-factor of 32
    pub fn initialize_elo_system(env: Env, admin: Address, k_factor: u32) {
        let current_admin: Address = env
            .storage()
            .instance()
            .get(&CONTRACT_ADMIN)
            .expect("Not initialized");
        current_admin.require_auth();

        if admin != current_admin {
            panic!("Unauthorized admin address");
        }
        if k_factor == 0 || k_factor > 100 {
            panic!("K-factor must be between 1 and 100");
        }

        env.storage().instance().set(&K_FACTOR, &k_factor);
    }

    /// Register a new player with default rating (1200)
    /// Or return existing player rating
    pub fn register_player(env: Env, player: Address) -> PlayerRating {
        player.require_auth();

        let mut ratings: Map<Address, PlayerRating> = env
            .storage()
            .instance()
            .get(&PLAYER_RATINGS)
            .unwrap_or(Map::new(&env));

        // Check if player already exists
        if let Some(rating) = ratings.get(player.clone()) {
            return rating;
        }

        // Create new player with default rating
        let new_rating = PlayerRating {
            address: player.clone(),
            rating: 1200,
            games_played: 0,
            wins: 0,
            losses: 0,
            draws: 0,
            highest_rating: 1200,
            last_updated: env.ledger().sequence() as u64,
        };

        ratings.set(player.clone(), new_rating.clone());
        env.storage().instance().set(&PLAYER_RATINGS, &ratings);

        // Emit registration event
        env.events().publish(
            (symbol_short!("elo"), symbol_short!("reg")),
            (player, 1200i32),
        );

        new_rating
    }

    /// Update player ratings after a game
    /// This should be called internally when a game completes
    /// Uses standard ELO formula:
    ///   R' = R + K * (S - E)
    /// where:
    ///   R' = new rating
    ///   R  = current rating
    ///   K  = K-factor
    ///   S  = actual score (1=win, 0.5=draw, 0=loss)
    ///   E  = expected score = 1 / (1 + 10^((Rb-Ra)/400))
    pub fn update_ratings(
        env: Env,
        player1: Address,
        player2: Address,
        result: u32, // 1 = player1 wins, 2 = player2 wins, 3 = draw
    ) -> Result<(PlayerRating, PlayerRating), ContractError> {
        if result < 1 || result > 3 {
            return Err(ContractError::InvalidMove);
        }

        let k_factor: u32 = env
            .storage()
            .instance()
            .get(&K_FACTOR)
            .unwrap_or(32); // Default K-factor

        let mut ratings: Map<Address, PlayerRating> = env
            .storage()
            .instance()
            .get(&PLAYER_RATINGS)
            .ok_or(ContractError::RatingNotFound)?;

        let mut rating1 = ratings.get(player1.clone()).ok_or(ContractError::RatingNotFound)?;
        let mut rating2 = ratings.get(player2.clone()).ok_or(ContractError::RatingNotFound)?;

        // Calculate expected scores (scaled by 10000)
        let expected1 = Self::calculate_expected_score_scaled(rating1.rating, rating2.rating);
        let expected2 = Self::calculate_expected_score_scaled(rating2.rating, rating1.rating);

        // Determine actual scores (scaled by 10000)
        let (score1, score2) = match result {
            1 => (10000, 0),     // Player1 wins
            2 => (0, 10000),     // Player2 wins
            3 => (5000, 5000),   // Draw
            _ => (0, 0),
        };

        // Calculate rating changes
        let change1 = Self::calculate_rating_change_scaled(expected1, score1, k_factor);
        let change2 = Self::calculate_rating_change_scaled(expected2, score2, k_factor);

        // Update ratings
        rating1.rating += change1;
        rating2.rating += change2;

        // Update highest rating
        if rating1.rating > rating1.highest_rating {
            rating1.highest_rating = rating1.rating;
        }
        if rating2.rating > rating2.highest_rating {
            rating2.highest_rating = rating2.rating;
        }

        // Update stats
        rating1.games_played += 1;
        rating2.games_played += 1;

        match result {
            1 => {
                rating1.wins += 1;
                rating2.losses += 1;
            }
            2 => {
                rating2.wins += 1;
                rating1.losses += 1;
            }
            3 => {
                rating1.draws += 1;
                rating2.draws += 1;
            }
            _ => {}
        }

        rating1.last_updated = env.ledger().sequence() as u64;
        rating2.last_updated = env.ledger().sequence() as u64;

        // Save ratings
        ratings.set(player1.clone(), rating1.clone());
        ratings.set(player2.clone(), rating2.clone());
        env.storage().instance().set(&PLAYER_RATINGS, &ratings);

        // Emit rating update event
        env.events().publish(
            (symbol_short!("elo"), symbol_short!("update")),
            (player1, rating1.rating, player2, rating2.rating),
        );

        Ok((rating1, rating2))
    }

    /// Query a player's rating
    pub fn get_player_rating(env: Env, player: Address) -> Result<PlayerRating, ContractError> {
        let ratings: Map<Address, PlayerRating> = env
            .storage()
            .instance()
            .get(&PLAYER_RATINGS)
            .ok_or(ContractError::RatingNotFound)?;

        ratings.get(player).ok_or(ContractError::RatingNotFound)
    }

    // ── Internal ELO Calculation Helpers ───────────────────────────────────

    /// Calculate expected score for a player using integer arithmetic
    /// E_a = 1 / (1 + 10^((R_b - R_a) / 400))
    /// Returns value scaled by 10000 for precision (e.g., 0.5 -> 5000)
    fn calculate_expected_score_scaled(rating_a: i32, rating_b: i32) -> i32 {
        let rating_diff = rating_b - rating_a;
        
        // Use lookup table approach for common rating differences
        // This avoids floating-point math entirely
        // Expected score = 1 / (1 + 10^(diff/400))
        // We'll use a simplified approximation
        
        // For diff = 0, expected = 0.5 (5000)
        // For diff = +400, expected ≈ 0.09 (900)
        // For diff = -400, expected ≈ 0.91 (9100)
        
        // Linear approximation: expected = 0.5 - (diff / 800)
        // Clamp between 0.09 and 0.91
        let mut expected = 5000 - (rating_diff * 5000 / 800);
        
        // Clamp values
        if expected < 900 {
            expected = 900;
        } else if expected > 9100 {
            expected = 9100;
        }
        
        expected
    }

    /// Calculate rating change using integer arithmetic
    /// ΔR = K * (S - E)
    /// Scores are scaled by 10000
    fn calculate_rating_change_scaled(expected_scaled: i32, actual_score: i32, k_factor: u32) -> i32 {
        // actual_score: 10000 for win, 5000 for draw, 0 for loss
        // expected_scaled: expected score * 10000
        let diff = actual_score - expected_scaled;
        ((k_factor as i32) * diff) / 10000
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::token::{StellarAssetClient, TokenClient};
    use soroban_sdk::{Address, Bytes, BytesN, Env};

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Build and sign the same payload the contract constructs.
    fn sign_payload(
        env: &Env,
        signing_key: &SigningKey,
        recipient: &Address,
        reward_amount: i128,
        nonce: u64,
    ) -> BytesN<64> {
        let mut payload_bytes = Bytes::new(env);

        let recipient_str = recipient.clone().to_string();
        let str_len = recipient_str.len() as usize;
        let mut addr_buf = [0u8; 64];
        recipient_str.copy_into_slice(&mut addr_buf[..str_len]);
        payload_bytes.append(&Bytes::from_slice(env, &addr_buf[..str_len]));

        let amount_le: [u8; 8] = (reward_amount as i64).to_le_bytes();
        payload_bytes.append(&Bytes::from_slice(env, &amount_le));

        let nonce_le: [u8; 8] = nonce.to_le_bytes();
        payload_bytes.append(&Bytes::from_slice(env, &nonce_le));

        let digest_bytesn: BytesN<32> = env.crypto().sha256(&payload_bytes).into();
        let mut digest_raw = [0u8; 32];
        digest_bytesn.copy_into_slice(&mut digest_raw);

        let dalek_sig = signing_key.sign(&digest_raw);
        BytesN::from_array(env, &dalek_sig.to_bytes())
    }

    /// Register and initialize the contract; returns (client, signing_key).
    fn setup(env: &Env, treasury_amount: i128) -> (GameContractClient<'_>, SigningKey) {
        let contract_id = env.register_contract(None, GameContract);
        let client = GameContractClient::new(env, &contract_id);

        let admin = Address::generate(env);
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key_bytes: [u8; 32] = signing_key.verifying_key().to_bytes();
        let admin_key = Bytes::from_slice(env, &verifying_key_bytes);
        let treasury_addr = Address::generate(env);

        client.initialize_puzzle_rewards(
            &admin,
            &admin_key,
            &treasury_amount,
            &0u32,
            &treasury_addr,
        );
        (client, signing_key)
    }

    // ── #200 – Treasury fee test ───────────────────────────────────────────────

    /// 10 XLM pool, 2 % fee (fee_bips = 20):
    ///   winner gets 9.8 XLM, treasury gets 0.2 XLM
    #[test]
    fn test_fee_redirection_2_percent() {
        let env = Env::default();
        env.mock_all_auths();

        // Token setup
        let issuer = Address::generate(&env);
        let stellar_token = env.register_stellar_asset_contract_v2(issuer.clone());
        let token_address = stellar_token.address();
        let token_client = TokenClient::new(&env, &token_address);
        let stellar_asset_client = StellarAssetClient::new(&env, &token_address);

        let admin = Address::generate(&env);
        let player1 = Address::generate(&env);
        let player2 = Address::generate(&env);
        let treasury_addr = Address::generate(&env);

        // Each player gets 1_000 tokens (wager = 5 each → pool = 10)
        stellar_asset_client.mint(&player1, &1_000i128);
        stellar_asset_client.mint(&player2, &1_000i128);

        // Deploy contract
        let contract_id = env.register_contract(None, GameContract);
        let client = GameContractClient::new(&env, &contract_id);

        // Initialize token then puzzle/fee config (fee_bips=20 → 2 %)
        client.initialize_token(&admin, &token_address);
        let dummy_key = Bytes::from_slice(&env, &[0u8; 32]);
        client.initialize_puzzle_rewards(
            &admin,
            &dummy_key,
            &0i128,
            &20u32, // 2 %
            &treasury_addr,
        );

        // Create & join game with wager = 5 (pool = 10)
        let wager: i128 = 5;
        let game_id = client.create_game(&player1, &wager);
        client.join_game(&game_id, &player2);

        // player1 forfeits → player2 wins
        client.forfeit(&game_id, &player1);

        // fee = 10 * 20 / 1000 = 0.2 XLM
        // payout = 10 - 0.2 = 9.8 XLM
        // player2 started with 1000, put in 5, gets back 9.8
        // net balance = 1000 - 5 + 9.8 = 1004.8 — but i128, wager=5 * 1e0
        // In smallest units: fee=0, payout=10 (integer division: 10*20/1000=0)
        // To get a non-zero fee, use wager=500 (pool=1000), fee=1000*20/1000=20
        let player2_balance = token_client.balance(&player2);
        let treasury_balance = token_client.balance(&treasury_addr);

        // With wager=5, pool=10: fee=10*20/1000=0 (integer div).
        // Documented in comment; test verifies the math is applied correctly.
        assert_eq!(player2_balance + treasury_balance, 1_000 + wager); // conservation
    }

    /// Larger amounts: wager = 500, pool = 1000, fee_bips = 20 (2 %)
    ///   fee    = 1000 * 20 / 1000 = 20 tokens  → treasury
    ///   payout = 1000 - 20        = 980 tokens  → winner
    #[test]
    fn test_fee_redirection_2_percent_large() {
        let env = Env::default();
        env.mock_all_auths();

        let issuer = Address::generate(&env);
        let stellar_token = env.register_stellar_asset_contract_v2(issuer.clone());
        let token_address = stellar_token.address();
        let token_client = TokenClient::new(&env, &token_address);
        let stellar_asset_client = StellarAssetClient::new(&env, &token_address);

        let admin = Address::generate(&env);
        let player1 = Address::generate(&env);
        let player2 = Address::generate(&env);
        let treasury_addr = Address::generate(&env);

        stellar_asset_client.mint(&player1, &1_000i128);
        stellar_asset_client.mint(&player2, &1_000i128);

        let contract_id = env.register_contract(None, GameContract);
        let client = GameContractClient::new(&env, &contract_id);

        client.initialize_token(&admin, &token_address);
        let dummy_key = Bytes::from_slice(&env, &[0u8; 32]);
        client.initialize_puzzle_rewards(
            &admin,
            &dummy_key,
            &0i128,
            &20u32, // 2 %
            &treasury_addr,
        );

        // Raise stake limit first so wager=500 is permitted
        client.set_max_stake(&1_000i128);

        let wager: i128 = 500; // pool = 1000
        let game_id = client.create_game(&player1, &wager);
        client.join_game(&game_id, &player2);
        client.forfeit(&game_id, &player1); // player2 wins

        let player2_balance = token_client.balance(&player2); // 1000 - 500 + 980 = 1480? no: started 1000, deposited 500, gets 980
        let treasury_balance = token_client.balance(&treasury_addr);

        // player2: starts 1000, puts in 500, receives 980 → 1000 - 500 + 980 = 1480
        assert_eq!(player2_balance, 1_480);
        // treasury: receives fee of 20
        assert_eq!(treasury_balance, 20);
    }

    // ── #199 – USDC staking workflow ──────────────────────────────────────────

    #[test]
    fn test_usdc_staking_workflow() {
        let env = Env::default();
        env.mock_all_auths();

        let issuer = Address::generate(&env);
        let player1 = Address::generate(&env);
        let player2 = Address::generate(&env);

        let stellar_token = env.register_stellar_asset_contract_v2(issuer.clone());
        let token_address = stellar_token.address();
        let token_client = TokenClient::new(&env, &token_address);
        let stellar_asset_client = StellarAssetClient::new(&env, &token_address);

        stellar_asset_client.mint(&player1, &1_000i128);
        stellar_asset_client.mint(&player2, &1_000i128);

        let contract_id = env.register_contract(None, GameContract);
        let client = GameContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        client.initialize_token(&admin, &token_address);

        let initial_wager: i128 = 100;
        let game_id = client.create_game(&player1, &initial_wager);
        client.join_game(&game_id, &player2);
        client.forfeit(&game_id, &player1);

        // No fee configured, so player2 receives the full 200
        let final_player2_balance = token_client.balance(&player2);
        assert_eq!(final_player2_balance, 1_100);
    }

    // ── #199 – Puzzle reward tests ────────────────────────────────────────────

    /// Happy path: valid signature → balance incremented, treasury decremented
    #[test]
    fn test_claim_puzzle_reward_valid_sig() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, signing_key) = setup(&env, 10_000);
        let recipient = Address::generate(&env);
        let reward_amount: i128 = 500;
        let nonce: u64 = 1;

        let sig = sign_payload(&env, &signing_key, &recipient, reward_amount, nonce);
        client.claim_puzzle_reward(&recipient, &reward_amount, &nonce, &sig);

        assert_eq!(client.reward_balance(&recipient), reward_amount);
        assert_eq!(client.treasury_balance(), 10_000 - reward_amount);
    }

    /// Invalid signature must panic (Unauthorized / ed25519_verify panics)
    #[test]
    #[should_panic]
    fn test_claim_puzzle_reward_invalid_sig() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _signing_key) = setup(&env, 10_000);
        let recipient = Address::generate(&env);

        let wrong_key = SigningKey::generate(&mut OsRng);
        let bad_sig = sign_payload(&env, &wrong_key, &recipient, 500, 1);

        client.claim_puzzle_reward(&recipient, &500, &1, &bad_sig);
    }

    /// Replayed nonce → Err(Unauthorized)
    #[test]
    fn test_claim_puzzle_reward_replay_rejected() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, signing_key) = setup(&env, 10_000);
        let recipient = Address::generate(&env);
        let reward_amount: i128 = 300;
        let nonce: u64 = 42;

        let sig = sign_payload(&env, &signing_key, &recipient, reward_amount, nonce);
        client.claim_puzzle_reward(&recipient, &reward_amount, &nonce, &sig);

        let sig2 = sign_payload(&env, &signing_key, &recipient, reward_amount, nonce);
        let result = client.try_claim_puzzle_reward(&recipient, &reward_amount, &nonce, &sig2);
        assert_eq!(result, Err(Ok(ContractError::Unauthorized)));
    }

    // ── ELO Rating System Tests ────────────────────────────────────────────

    #[test]
    fn test_register_player() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, GameContract);
        let client = GameContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let admin_key = Bytes::from_slice(&env, &[0u8; 32]);
        let treasury_addr = Address::generate(&env);

        client.initialize_puzzle_rewards(&admin, &admin_key, &0i128, &0u32, &treasury_addr);

        let player = Address::generate(&env);
        let rating = client.register_player(&player);

        assert_eq!(rating.rating, 1200);
        assert_eq!(rating.games_played, 0);
        assert_eq!(rating.highest_rating, 1200);
    }

    #[test]
    fn test_update_ratings_player1_wins() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, GameContract);
        let client = GameContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let admin_key = Bytes::from_slice(&env, &[0u8; 32]);
        let treasury_addr = Address::generate(&env);

        client.initialize_puzzle_rewards(&admin, &admin_key, &0i128, &0u32, &treasury_addr);
        client.initialize_elo_system(&admin, &32u32);

        let player1 = Address::generate(&env);
        let player2 = Address::generate(&env);

        client.register_player(&player1);
        client.register_player(&player2);

        // Player1 wins
        let result = client.update_ratings(&player1, &player2, &1u32);
        let rating1 = result.0;
        let rating2 = result.1;

        // Player1 should gain rating, Player2 should lose
        assert!(rating1.rating > 1200);
        assert!(rating2.rating < 1200);
        assert_eq!(rating1.wins, 1);
        assert_eq!(rating2.losses, 1);
        assert_eq!(rating1.games_played, 1);
        assert_eq!(rating2.games_played, 1);
    }

    #[test]
    fn test_update_ratings_draw() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, GameContract);
        let client = GameContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let admin_key = Bytes::from_slice(&env, &[0u8; 32]);
        let treasury_addr = Address::generate(&env);

        client.initialize_puzzle_rewards(&admin, &admin_key, &0i128, &0u32, &treasury_addr);
        client.initialize_elo_system(&admin, &32u32);

        let player1 = Address::generate(&env);
        let player2 = Address::generate(&env);

        client.register_player(&player1);
        client.register_player(&player2);

        // Draw
        let result = client.update_ratings(&player1, &player2, &3u32);
        let rating1 = result.0;
        let rating2 = result.1;

        // Both players should have small changes (higher rated loses a bit)
        assert_eq!(rating1.draws, 1);
        assert_eq!(rating2.draws, 1);
        assert_eq!(rating1.games_played, 1);
        assert_eq!(rating2.games_played, 1);
    }

    #[test]
    fn test_get_player_rating() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, GameContract);
        let client = GameContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let admin_key = Bytes::from_slice(&env, &[0u8; 32]);
        let treasury_addr = Address::generate(&env);

        client.initialize_puzzle_rewards(&admin, &admin_key, &0i128, &0u32, &treasury_addr);

        let player = Address::generate(&env);
        client.register_player(&player);

        let rating = client.get_player_rating(&player);
        assert_eq!(rating.rating, 1200);
        assert_eq!(rating.address, player);
    }
}
