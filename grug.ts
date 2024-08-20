/*
 Generated by typeshare 1.10.0-beta.7
*/

/**
 * An account address.
 * 
 * In Grug, addresses are of 20-byte length, in Hex encoding and the `0x` prefix.
 * In comparison, in the "vanilla" CosmWasm, addresses are either 20- or 32-byte,
 * in Bech32 encoding. The last 6 ASCII characters are the checksum.
 * 
 * In CosmWasm, when addresses are deserialized from JSON, no validation is
 * performed. An attacker can put a string that is not a valid address in a
 * message, and this would be deserialized into an `cosmwasm_std::Addr` without
 * error. Therefore, in CosmWasm, it is recommended to deserialize addresses
 * into `String`s first, then call `deps.api.addr_validate` to validate them.
 * This can be sometimes very cumbersome. It may be necessary to define two
 * versions of the same type, one "unchecked" version with `String`s, one
 * "checked" version with `Addr`s.
 * 
 * In Grug, addresses are validated during deserialization. If deserialization
 * doesn't throw an error, you can be sure the address is valid. Therefore it
 * is safe to use `Addr`s in JSON messages.
 */
export type Addr = string;

export type Binary = number[];

/** A sorted list of coins or tokens. */
export type Coins = Record<string, string>;

/**
 * A span of time, in nanosecond precision.
 * 
 * We can't use [`std::time::Duration`](std::time::Duration) because it doesn't
 * implement the Borsh traits. Additionally, it's serialized to JSON as a
 * struct, e.g. `{"seconds":123,"nanos":123}`, which isn't desirable.
 */
export type Duration = Uint128;

/**
 * Set of updates to be made to the config.
 * 
 * A field being `Some` means it is to be updated to be the given value;
 * it being `None` means it is not to be updated.
 */
export interface ConfigUpdates {
	owner?: Addr;
	bank?: Addr;
	taxman?: Addr;
	cronjobs?: Record<Addr, Duration>;
	permissions?: Permissions;
}

export type Message = 
	/**
	 * Update the chain- and app-level configurations.
	 * 
	 * Only the `owner` is authorized to do this.
	 */
	| { type: "configure", content: {
	updates: ConfigUpdates;
	app_updates: Record<string, any>;
}}
	/** Send coins to the given recipient address. */
	| { type: "transfer", content: {
	to: Addr;
	coins: Coins;
}}
	/** Upload a Wasm binary code and store it in the chain's state. */
	| { type: "upload", content: {
	code: Binary;
}}
	/** Register a new account. */
	| { type: "instantiate", content: {
	code_hash: string;
	msg: any;
	salt: Binary;
	funds: Coins;
	admin?: Addr;
}}
	/** Execute a contract. */
	| { type: "execute", content: {
	contract: Addr;
	msg: any;
	funds: Coins;
}}
	/**
	 * Update the `code_hash` associated with a contract.
	 * 
	 * Only the contract's `admin` is authorized to do this. If the admin is
	 * set to `None`, no one can update the code hash.
	 */
	| { type: "migrate", content: {
	contract: Addr;
	new_code_hash: string;
	msg: any;
}};

export interface Tx {
	sender: Addr;
	gas_limit: number;
	msgs: Message[];
	data: any;
	credential: any;
}

