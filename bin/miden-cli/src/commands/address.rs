use clap::ValueEnum;
use miden_client::Client;
use miden_client::account::AccountId;
use miden_client::address::{Address, AddressId, AddressInterface, NetworkId, RoutingParameters};

use crate::errors::CliError;
use crate::utils::parse_account_id;
use crate::{Parser, Subcommand, create_dynamic_table};

/// Mirrors [`AddressInterface`], enabling parsing for CLI commands.
///
/// An interface specifies the set of procedures an account exposes, which determines
/// which notes it is able to receive and consume.
#[derive(Debug, Clone, ValueEnum)]
pub enum CliAddressInterface {
    BasicWallet,
}

impl From<CliAddressInterface> for AddressInterface {
    fn from(value: CliAddressInterface) -> Self {
        match value {
            CliAddressInterface::BasicWallet => AddressInterface::BasicWallet,
        }
    }
}

#[derive(Debug, Subcommand, Clone)]
pub enum AddressSubCommand {
    /// List all addresses an account can be referenced by
    List { account_id: Option<String> },
    /// Add a previously-encoded address to an account.
    ///
    /// To produce the bech32 `ADDRESS` argument from its fields, see the `encode` subcommand.
    Add {
        /// Account to add the address to
        account_id: String,
        /// Bech32-encoded address to track
        address: String,
    },
    /// Remove the given address
    Remove {
        /// Account that owns the address to remove
        account_id: String,
        /// Address to remove
        address: String,
    },
    /// Encode an address from its fields and print it as a bech32 string.
    ///
    /// The network HRP used for the encoding is taken from the CLI configuration.
    Encode {
        /// Account that the address points to
        account_id: String,
        /// Interface the address exposes.
        #[arg(value_enum)]
        interface: CliAddressInterface,
        /// Optional tag length
        tag_len: Option<u8>,
    },
}

#[derive(Debug, Parser, Clone)]
#[command(about = "Manage account addresses")]
pub struct AddressCmd {
    #[clap(subcommand)]
    command: Option<AddressSubCommand>,
}

impl AddressCmd {
    pub async fn execute<AUTH>(&self, client: Client<AUTH>) -> Result<(), CliError> {
        let network_id = client.network_id().await?;
        match &self.command {
            Some(AddressSubCommand::List { account_id: Some(account_id) }) => {
                list_account_addresses(client, account_id, network_id).await?;
            },
            Some(AddressSubCommand::Add { account_id, address }) => {
                add_address(client, account_id.clone(), address.clone(), network_id).await?;
            },
            Some(AddressSubCommand::Remove { account_id, address }) => {
                remove_address(client, account_id.clone(), address.clone(), network_id).await?;
            },
            Some(AddressSubCommand::Encode { account_id, interface, tag_len }) => {
                encode_address(client, account_id, interface.clone(), *tag_len, network_id).await?;
            },
            _ => {
                // List all addresses as default
                list_all_addresses(client, network_id).await?;
            },
        }
        Ok(())
    }
}

// HELPERS
// ================================================================================================

fn print_account_addresses(account_id: &String, addresses: &Vec<Address>, network_id: &NetworkId) {
    println!("Addresses for AccountId {account_id}:");
    let mut table = create_dynamic_table(&["Address", "Interface"]);
    for address in addresses {
        let address_bech32 = address.encode(network_id.clone());
        let interface = match address.interface() {
            Some(interface) => interface.to_string(),
            None => "Unspecified".to_string(),
        };
        table.add_row(vec![address_bech32, interface]);
    }

    println!("{table}");
}

async fn list_all_addresses<AUTH>(
    client: Client<AUTH>,
    network_id: NetworkId,
) -> Result<(), CliError> {
    println!("Listing addresses for all accounts:\n");
    let accounts = client.get_account_headers().await?;
    for (acc_header, _) in accounts {
        let addresses = client
            .account_reader(acc_header.id())
            .addresses()
            .await
            .expect("account is expected to exist if retrieved");
        print_account_addresses(&acc_header.id().to_string(), &addresses, &network_id);
        println!();
    }
    Ok(())
}

async fn list_account_addresses<AUTH>(
    client: Client<AUTH>,
    account_id: &String,
    network_id: NetworkId,
) -> Result<(), CliError> {
    let id = parse_account_id(&client, account_id).await?;
    let addresses = client.account_reader(id).addresses().await.map_err(|_| {
        CliError::Input(format!("The account with id `{account_id}` does not exist"))
    })?;

    print_account_addresses(&id.to_hex(), &addresses, &network_id);
    Ok(())
}

async fn add_address<AUTH>(
    mut client: Client<AUTH>,
    account_id: String,
    address: String,
    network_id: NetworkId,
) -> Result<(), CliError> {
    let account_id = parse_account_id(&client, &account_id).await?;
    let address = decode_account_address(&address, account_id, &network_id)?;

    let note_tag = address.to_note_tag();
    client.add_address(address, account_id).await?;

    println!("Address added: Account Id {account_id} - Note tag: {note_tag}");
    Ok(())
}

async fn remove_address<AUTH>(
    mut client: Client<AUTH>,
    account_id: String,
    address: String,
    network_id: NetworkId,
) -> Result<(), CliError> {
    let account_id = parse_account_id(&client, &account_id).await?;
    let address = decode_account_address(&address, account_id, &network_id)?;
    let note_tag = address.to_note_tag();

    if client.remove_address(address, account_id).await? {
        println!("Address removed - Account Id {account_id} - Note tag: {note_tag}");
    } else {
        println!("No address was tracked for Account Id {account_id}");
    }

    Ok(())
}

async fn encode_address<AUTH>(
    client: Client<AUTH>,
    account_id: &str,
    interface: CliAddressInterface,
    tag_len: Option<u8>,
    network_id: NetworkId,
) -> Result<(), CliError> {
    let account_id = parse_account_id(&client, account_id).await?;
    let interface = interface.into();
    let routing_params = match tag_len {
        Some(tag_len) => RoutingParameters::new(interface)
            .with_note_tag_len(tag_len)
            .map_err(|e| CliError::Address(e, String::new()))?,
        None => RoutingParameters::new(interface),
    };
    let address = Address::new(account_id).with_routing_parameters(routing_params);

    println!("{}", address.encode(network_id));
    Ok(())
}

/// Decodes a bech32 address and verifies it encodes the expected account ID and network.
fn decode_account_address(
    encoded: &str,
    expected_account_id: AccountId,
    expected_network_id: &NetworkId,
) -> Result<Address, CliError> {
    let (decoded_network_id, address) =
        Address::decode(encoded).map_err(|e| CliError::Address(e, encoded.to_string()))?;

    let AddressId::AccountId(address_account_id) = address.id() else {
        return Err(CliError::Input(
            "Address is not account-ID-based; only account-ID addresses can be tracked".to_string(),
        ));
    };
    if address_account_id != expected_account_id {
        return Err(CliError::Input(format!(
            "Address encodes account ID `{}` which does not match the provided account ID `{}`",
            address_account_id.to_hex(),
            expected_account_id.to_hex(),
        )));
    }

    if &decoded_network_id != expected_network_id {
        return Err(CliError::Input(format!(
            "Address network `{decoded_network_id}` does not match configured network `{expected_network_id}`",
        )));
    }

    Ok(address)
}
