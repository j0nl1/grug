use {
    crate::prompt::{confirm, print_json_pretty, read_password},
    anyhow::anyhow,
    clap::Parser,
    colored::Colorize,
    cw_rs::{Client, SigningKey, SigningOptions},
    cw_std::{from_json, hash, Addr, Binary, Coins, Config, Hash, Message},
    serde::Serialize,
    std::{fs::File, io::Read, path::PathBuf, str::FromStr},
    tendermint_rpc::endpoint::broadcast::tx_sync,
};

#[derive(Parser)]
pub enum TxCmd {
    /// Update the chain-level configurations
    UpdateConfig {
        /// New configurations as a JSON string
        new_cfg: String,
    },
    /// Send coins to the given recipient address
    Transfer {
        /// Recipient address
        to: Addr,
        /// Coins to send in the format: {denom1}:{amount},{denom2}:{amount},...
        coins: String,
    },
    /// Update a Wasm binary code
    Store {
        /// Path to the Wasm file
        path: PathBuf,
    },
    /// Instantiate a new contract
    Instantiate {
        /// Hash of the Wasm byte code to be associated with the contract
        code_hash: Hash,
        /// Instantiate message as a JSON string
        msg: String,
        /// Salt in UTF-8 encoding
        salt: String,
        /// Coins to be sent to the contract, in the format: {denom1}:{amount},{denom2}:{amount},...
        #[arg(long)]
        funds: Option<String>,
        /// Administrator address for the contract
        #[arg(long)]
        admin: Option<Addr>,
    },
    /// Upload code and instantiate a contract in one go
    StoreAndInstantiate {
        /// Path to the Wasm file
        path: PathBuf,
        /// Instantiate message as a JSON string
        msg: String,
        /// Salt in UTF-8 encoding
        salt: String,
        /// Coins to be sent to the contract, in the format: {denom1}:{amount},{denom2}:{amount},...
        #[arg(long)]
        funds: Option<String>,
        /// Administrator address for the contract
        #[arg(long)]
        admin: Option<Addr>,
    },
    /// Execute a contract
    Execute {
        /// Contract address
        contract: Addr,
        /// Execute message as a JSON string
        msg: String,
        /// Coins to be sent to the contract, in the format: {denom1}:{amount},{denom2}:{amount},...
        #[arg(long)]
        funds: Option<String>,
    },
    /// Update the code hash associated with a contract
    Migrate {
        /// Contract address
        contract: Addr,
        /// New code hash
        new_code_hash: Hash,
        /// Migrate message as a JSON string
        msg: String,
    },
    /// Create an IBC light client
    CreateClient {
        /// Hash of the Wasm byte code to be associated with the contract
        code_hash: Hash,
        /// Client state as a JSON string
        client_state: String,
        /// Consensus state as a JSON string
        consensus_state: String,
        /// Salt in UTF-8 encoding
        salt: String,
    },
    /// Update the state of an IBC light client
    UpdateClient {
        /// Address of the client contract
        client: Addr,
        /// Block header as a JSON string
        header: String,
    },
    /// Submit proof of a misbehavior to an IBC light client
    SubmitMishebavior {
        /// Address of the client contract
        client: Addr,
        /// Misbehavior as a JSON string
        misbehavior: String,
    },
}

impl TxCmd {
    pub async fn run(
        self,
        rpc_addr: &str,
        key_dir: PathBuf,
        key_name: Option<String>,
        sender: Option<Addr>,
        chain_id: Option<String>,
        sequence: Option<u32>,
    ) -> anyhow::Result<()> {
        let sender = sender.ok_or(anyhow!("sender not specified"))?;
        let key_name = key_name.ok_or(anyhow!("key name not specified"))?;

        // compose the message
        let msgs = match self {
            TxCmd::UpdateConfig { new_cfg } => {
                let new_cfg: Config = from_json(new_cfg.as_bytes())?;
                vec![Message::UpdateConfig {
                    new_cfg,
                }]
            },
            TxCmd::Transfer { to, coins } => {
                let coins = Coins::from_str(&coins)?;
                vec![Message::Transfer {
                    to,
                    coins,
                }]
            },
            TxCmd::Store { path } => {
                let mut file = File::open(path)?;
                let mut wasm_byte_code = vec![];
                file.read_to_end(&mut wasm_byte_code)?;
                vec![Message::StoreCode {
                    wasm_byte_code: wasm_byte_code.into(),
                }]
            },
            TxCmd::Instantiate { code_hash, msg, salt, funds, admin } => {
                vec![Message::Instantiate {
                    msg:   msg.into_bytes().into(),
                    salt:  salt.into_bytes().into(),
                    funds: Coins::from_str(&funds.unwrap_or_default())?,
                    code_hash,
                    admin,
                }]
            },
            TxCmd::StoreAndInstantiate { path, msg, salt, funds, admin } => {
                let mut file = File::open(path)?;
                let mut wasm_byte_code = vec![];
                file.read_to_end(&mut wasm_byte_code)?;
                let code_hash = hash(&wasm_byte_code);
                vec![
                    Message::StoreCode {
                        wasm_byte_code: wasm_byte_code.into(),
                    },
                    Message::Instantiate {
                        msg:   msg.into_bytes().into(),
                        salt:  salt.into_bytes().into(),
                        funds: Coins::from_str(funds.as_deref().unwrap_or(Coins::EMPTY_COINS_STR))?,
                        code_hash,
                        admin,
                    },
                ]
            },
            TxCmd::Execute { contract, msg, funds } => {
                vec![Message::Execute {
                    msg:   msg.into_bytes().into(),
                    funds: Coins::from_str(funds.as_deref().unwrap_or(Coins::EMPTY_COINS_STR))?,
                    contract,
                }]
            },
            TxCmd::Migrate { contract, new_code_hash, msg } => {
                vec![Message::Migrate {
                    msg: msg.into_bytes().into(),
                    new_code_hash,
                    contract,
                }]
            },
            TxCmd::CreateClient { code_hash, client_state, consensus_state, salt } => {
                vec![Message::CreateClient {
                    code_hash,
                    client_state:    client_state.into_bytes().into(),
                    consensus_state: consensus_state.into_bytes().into(),
                    salt:            salt.into_bytes().into(),
                }]
            },
            TxCmd::UpdateClient { client, header } => {
                vec![Message::UpdateClient {
                    client,
                    header: header.into_bytes().into(),
                }]
            },
            TxCmd::SubmitMishebavior { client, misbehavior } => {
                vec![Message::SubmitMisbehavior {
                    client,
                    misbehavior: misbehavior.into_bytes().into(),
                }]
            },
        };

        // load signing key
        let key_path = key_dir.join(format!("{key_name}.json"));
        let password = read_password("🔑 Enter a password to encrypt the key".bold())?;
        let signing_key = SigningKey::from_file(&key_path, &password)?;
        let sign_opts = SigningOptions {
            signing_key,
            sender,
            chain_id,
            sequence,
        };

        // broadcast transaction
        let client = Client::connect(rpc_addr)?;
        let maybe_res = client.send_tx_with_confirmation(msgs, &sign_opts, |tx| {
            print_json_pretty(tx)?;
            Ok(confirm("🤔 Broadcast transaction?".bold())?)
        })
        .await?;

        // print result
        if let Some(res) = maybe_res {
            print_json_pretty(PrintableBroadcastResponse::from(res))?;
        } else {
            println!("🤷 User aborted");
        }

        Ok(())
    }
}

/// Similar to tendermint_rpc Response but serializes to nicer JSON.
#[derive(Serialize)]
struct PrintableBroadcastResponse {
    code: u32,
    data: Binary,
    log:  String,
    hash: String,
}

impl From<tx_sync::Response> for PrintableBroadcastResponse {
    fn from(broadcast_res: tx_sync::Response) -> Self {
        Self {
            code: broadcast_res.code.into(),
            data: broadcast_res.data.to_vec().into(),
            log:  broadcast_res.log,
            hash: broadcast_res.hash.to_string(),
        }
    }
}
