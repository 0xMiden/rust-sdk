//! Decoding of standard note storage and attachments.

use miden_client::account::FaucetMetadata;
use miden_client::asset::{Asset, AssetAmount, FungibleAsset};
use miden_client::block::BlockNumber;
use miden_client::note::standards::{
    FeeSponsorshipNoteStorage,
    NoteExecutionHint,
    PswapNoteAttachment,
    PswapNoteStorage,
    StandardNoteAttachment,
    SwapNoteStorage,
    SwapPayback,
};
use miden_client::note::{
    NetworkAccountTarget,
    NoteAttachment,
    NoteAttachmentScheme,
    P2idNoteStorage,
    P2ideNoteStorage,
    StandardNote,
};
use miden_client::{Client, Felt, Word};

use crate::errors::CliError;
use crate::utils::{FaucetMetadataResolver, NO_VALUE, base_units_to_tokens};

// NOTE STORAGE DECODING
// ================================================================================================

/// Decodes the storage of a P2ID, P2IDE, SWAP, PSWAP or `FEE_SPONSORSHIP` note into named fields.
///
/// Returns `None` for other notes and for storage that does not decode.
async fn decode_standard_note_storage<AUTH>(
    client: &Client<AUTH>,
    resolver: &FaucetMetadataResolver,
    standard_note: Option<StandardNote>,
    items: &[Felt],
) -> Result<Option<Vec<(&'static str, String)>>, CliError> {
    let fields = match standard_note {
        Some(StandardNote::P2ID) => P2idNoteStorage::try_from(items)
            .map(|storage| vec![("target", storage.target().to_string())])
            .ok(),
        Some(StandardNote::P2IDE) => P2ideNoteStorage::try_from(items)
            .map(|storage| {
                vec![
                    ("target", storage.target().to_string()),
                    ("reclaimer", storage.reclaimer().to_string()),
                    ("reclaim height", format_optional_height(storage.reclaim_height())),
                    ("timelock height", format_optional_height(storage.timelock_height())),
                ]
            })
            .ok(),
        Some(StandardNote::SWAP) => match SwapNoteStorage::try_from(items) {
            Ok(storage) => {
                let requested = resolver.format_asset(client, &storage.requested_asset()).await?;
                let payback = match storage.payback() {
                    SwapPayback::Public { payback_target_id } => {
                        ("payback target", payback_target_id.to_string())
                    },
                    SwapPayback::Private { recipient } => ("payback recipient", recipient.to_hex()),
                };
                Some(vec![
                    ("requested", requested.to_string()),
                    ("payback note", storage.payback_note_type().to_string()),
                    ("payback tag", storage.payback_tag().to_string()),
                    payback,
                ])
            },
            Err(_) => None,
        },
        Some(StandardNote::PSWAP) => match PswapNoteStorage::try_from(items) {
            Ok(storage) => {
                let requested = Asset::from(*storage.min_requested_asset());
                let fill_step = storage.min_fill_step();
                // A zero fill step means that the note accepts fills of any size.
                let fill_step = if fill_step == AssetAmount::ZERO {
                    NO_VALUE.to_string()
                } else {
                    match FungibleAsset::new(storage.requested_faucet_id(), fill_step.as_u64()) {
                        Ok(asset) => {
                            resolver.format_asset(client, &Asset::from(asset)).await?.to_string()
                        },
                        Err(_) => fill_step.as_u64().to_string(),
                    }
                };
                Some(vec![
                    ("creator", storage.creator_account_id().to_string()),
                    ("requested", resolver.format_asset(client, &requested).await?.to_string()),
                    ("min fill step", fill_step),
                    ("payback note", storage.payback_note_type().to_string()),
                ])
            },
            Err(_) => None,
        },
        Some(StandardNote::FEE_SPONSORSHIP) => FeeSponsorshipNoteStorage::try_from(items)
            .map(|storage| {
                vec![
                    ("feature note", storage.feature_note_id().to_hex()),
                    ("reclaimer", storage.reclaimer().to_string()),
                    ("reclaim height", format_optional_height(storage.reclaim_height())),
                ]
            })
            .ok(),
        _ => None,
    };

    Ok(fields)
}

/// Returns the rows that show note storage and the label of their first column.
///
/// A standard note gets named fields. Other notes get raw items by index.
pub(crate) async fn note_storage_rows<AUTH>(
    client: &Client<AUTH>,
    resolver: &FaucetMetadataResolver,
    standard_note: Option<StandardNote>,
    items: &[Felt],
) -> Result<(&'static str, Vec<(String, String)>), CliError> {
    if let Some(fields) =
        decode_standard_note_storage(client, resolver, standard_note, items).await?
    {
        let rows = fields.into_iter().map(|(name, value)| (name.to_string(), value)).collect();
        return Ok(("Field", rows));
    }
    let rows = items.iter().enumerate().map(|(idx, item)| (idx.to_string(), item.to_string()));
    Ok(("Index", rows.collect()))
}

/// Formats note storage for one table cell, with one `name: value` line for each row.
pub(crate) async fn format_note_storage<AUTH>(
    client: &Client<AUTH>,
    resolver: &FaucetMetadataResolver,
    standard_note: Option<StandardNote>,
    items: &[Felt],
) -> Result<String, CliError> {
    let (_, rows) = note_storage_rows(client, resolver, standard_note, items).await?;
    Ok(field_lines(rows))
}

/// Joins rows into `name: value` lines. Returns the placeholder if there are no rows.
fn field_lines<N: core::fmt::Display, V: core::fmt::Display>(
    rows: impl IntoIterator<Item = (N, V)>,
) -> String {
    let lines: Vec<String> =
        rows.into_iter().map(|(name, value)| format!("{name}: {value}")).collect();
    if lines.is_empty() {
        return NO_VALUE.to_string();
    }
    lines.join("\n")
}

/// Formats an optional block height.
fn format_optional_height(height: Option<BlockNumber>) -> String {
    height.map_or_else(|| NO_VALUE.to_string(), |height| height.to_string())
}

// NOTE ATTACHMENT DECODING
// ================================================================================================

/// Returns the name of a standard attachment scheme, or `None` for other schemes.
pub(crate) fn standard_attachment_name(scheme: NoteAttachmentScheme) -> Option<&'static str> {
    [
        (StandardNoteAttachment::NetworkAccountTarget, "network account target"),
        (StandardNoteAttachment::PswapAttachment, "PSWAP"),
        (StandardNoteAttachment::UsdcxMint, "USDCx mint"),
        (StandardNoteAttachment::UsdcxBurn, "USDCx burn"),
        (StandardNoteAttachment::AccountCodeUpgrade, "account code upgrade"),
    ]
    .into_iter()
    .find(|(standard, _)| standard.attachment_scheme() == scheme)
    .map(|(_, name)| name)
}

/// Decodes a network account target or PSWAP attachment into named fields.
///
/// The PSWAP amount is in units of the faucet of `amount_metadata`. If `amount_metadata` is `None`,
/// the amount is shown in base units.
///
/// Returns `None` for other attachments and for content that does not decode.
fn decode_standard_note_attachment(
    attachment: &NoteAttachment,
    amount_metadata: Option<&FaucetMetadata>,
) -> Option<Vec<(&'static str, String)>> {
    if let Ok(target) = NetworkAccountTarget::try_from(attachment) {
        let hint = match target.execution_hint() {
            NoteExecutionHint::None => NO_VALUE.to_string(),
            NoteExecutionHint::Always => "always".to_string(),
            NoteExecutionHint::AfterBlock { block_num } => format!("after block {block_num}"),
            NoteExecutionHint::OnBlockSlot { round_len, slot_len, slot_offset } => format!(
                "slot {slot_offset} of 2^{slot_len} blocks in a round of 2^{round_len} blocks"
            ),
            NoteExecutionHint::Unknown(felt) => format!("unknown ({felt})"),
        };
        return Some(vec![("target", target.target_id().to_string()), ("execution hint", hint)]);
    }

    PswapNoteAttachment::try_from(attachment).ok().map(|pswap| {
        let amount = match amount_metadata {
            Some(meta) => {
                format!("{} {}", base_units_to_tokens(pswap.amount(), meta.decimals), meta.symbol)
            },
            None => format!("{} base units", pswap.amount()),
        };
        vec![
            ("amount", amount),
            ("order id", pswap.order_id().to_string()),
            ("depth", pswap.depth().to_string()),
        ]
    })
}

/// Formats attachment content for one table cell: named fields for a standard attachment, and raw
/// words otherwise.
pub(crate) fn format_attachment_content(
    attachment: &NoteAttachment,
    amount_metadata: Option<&FaucetMetadata>,
) -> String {
    match decode_standard_note_attachment(attachment, amount_metadata) {
        Some(fields) => field_lines(fields),
        None => attachment
            .content()
            .as_words()
            .iter()
            .map(Word::to_hex)
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_client::account::{AccountId, FaucetMetadata};
    use miden_client::asset::AssetAmount;
    use miden_client::note::standards::{
        NoteExecutionHint,
        PswapNoteAttachment,
        StandardNoteAttachment,
    };
    use miden_client::note::{NetworkAccountTarget, NoteAttachment, NoteAttachmentScheme};
    use miden_client::testing::account_id::ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET;
    use miden_client::{Felt, Word};

    use super::{decode_standard_note_attachment, standard_attachment_name};

    fn amount(units: u64) -> AssetAmount {
        AssetAmount::new(units).unwrap()
    }

    // NOTE ATTACHMENT DECODING
    // --------------------------------------------------------------------------------------------

    #[test]
    fn standard_attachment_name_names_standard_schemes_only() {
        assert_eq!(
            standard_attachment_name(StandardNoteAttachment::PswapAttachment.attachment_scheme()),
            Some("PSWAP")
        );
        assert_eq!(standard_attachment_name(NoteAttachmentScheme::new(1000).unwrap()), None);
    }

    #[test]
    fn decode_standard_note_attachment_decodes_pswap_fields() {
        // Values from a real PSWAP remainder note.
        let order_id = Felt::new(4_216_412_421_694_694_733).unwrap();
        let attachment = NoteAttachment::from(PswapNoteAttachment::new(amount(40), order_id, 1));
        let metadata = FaucetMetadata { symbol: "BTC".to_string(), decimals: 10 };

        assert_eq!(
            decode_standard_note_attachment(&attachment, Some(&metadata)),
            Some(vec![
                ("amount", "0.0000000040 BTC".to_string()),
                ("order id", "4216412421694694733".to_string()),
                ("depth", "1".to_string()),
            ])
        );
        assert_eq!(
            decode_standard_note_attachment(&attachment, None),
            Some(vec![
                ("amount", "40 base units".to_string()),
                ("order id", "4216412421694694733".to_string()),
                ("depth", "1".to_string()),
            ])
        );
    }

    #[test]
    fn decode_standard_note_attachment_decodes_network_account_target_fields() {
        let target = AccountId::try_from(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET).unwrap();
        let hint = NoteExecutionHint::AfterBlock { block_num: 7u32.into() };
        let attachment = NoteAttachment::from(NetworkAccountTarget::new(target, hint).unwrap());

        assert_eq!(
            decode_standard_note_attachment(&attachment, None),
            Some(vec![
                ("target", target.to_string()),
                ("execution hint", "after block 7".to_string()),
            ])
        );
    }

    #[test]
    fn decode_standard_note_attachment_returns_none_for_other_content() {
        // Non-zero padding is not a valid PSWAP attachment.
        let content = Word::from([40u32, 1, 1, 1]);
        let attachment = NoteAttachment::with_word(
            StandardNoteAttachment::PswapAttachment.attachment_scheme(),
            content,
        );
        assert_eq!(decode_standard_note_attachment(&attachment, None), None);
    }
}
