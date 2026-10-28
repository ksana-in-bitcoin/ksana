//! Connects the execution layer's state buffer to the VM's [`Host`] interface. Every change to a built-in asset that
//! succeeds is recorded for the op's receipt (D-122).

use std::sync::Arc;

use ksana_primitives::asset::{DEFAULT_CAPABILITIES, STATUS_ALLOWED, STATUS_FROZEN};
use ksana_vm::{
    AssetMetaInput, AssetPolicy, ContractCode, Host, HostError, NativeOutput, NativeTxInfo,
};

use crate::builtin::{self, Fault};
use crate::keys::{self, ContractMeta};
use crate::state::{ChangeKind, Event, Overlay};
use crate::BtcView;

/// The host handed to the VM during one contract call (owns the state buffer and hands it back when the call ends).
pub(crate) struct OpHost {
    pub ov: Overlay,
    pub height: u64,
    pub timestamp: u64,
    pub anchor: u32,
    pub btc: Arc<dyn BtcView>,
    /// The BTC transaction ID carrying the path-C op (D-078); `None` for the other paths.
    pub native: Option<[u8; 32]>,
    /// The native transaction already looked up: multiple `native_tx` calls within one op query the BTC view only
    /// once.
    pub native_cache: Option<NativeTxInfo>,
}

fn host_err(f: Fault) -> HostError {
    match f {
        Fault::Failed(m) => HostError::Failed(m),
        // There is no "invalid block" case during a contract call; if one shows up it is an internal error.
        Fault::Invalid(m) | Fault::Fatal(m) => HostError::Fatal(m),
    }
}

impl Host for OpHost {
    fn storage_get(
        &mut self,
        contract: &[u8; 32],
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, HostError> {
        self.ov
            .get(&keys::storage(contract, key))
            .map_err(HostError::Fatal)
    }

    fn storage_set(
        &mut self,
        contract: &[u8; 32],
        key: &[u8],
        value: &[u8],
    ) -> Result<(), HostError> {
        self.ov.set(keys::storage(contract, key), value.to_vec());
        Ok(())
    }

    fn storage_remove(&mut self, contract: &[u8; 32], key: &[u8]) -> Result<(), HostError> {
        self.ov.delete(keys::storage(contract, key));
        Ok(())
    }

    fn block_height(&self) -> u64 {
        self.height
    }

    fn block_timestamp(&self) -> u64 {
        self.timestamp
    }

    fn btc_anchor_height(&self) -> u32 {
        self.anchor
    }

    fn btc_header(&mut self, height: u32) -> Result<Option<[u8; 80]>, HostError> {
        self.btc.header(height).map_err(HostError::Fatal)
    }

    fn native_tx(&mut self) -> Result<Option<NativeTxInfo>, HostError> {
        let Some(txid) = self.native else {
            return Ok(None);
        };
        if let Some(cached) = &self.native_cache {
            return Ok(Some(cached.clone()));
        }
        let outputs = self
            .btc
            .native_outputs(&txid)
            .map_err(|e| HostError::Fatal(format!("BTC data source failure: {e}")))?;
        let tx = NativeTxInfo {
            txid,
            outputs: outputs
                .into_iter()
                .map(|(value, script)| NativeOutput { value, script })
                .collect(),
        };
        self.native_cache = Some(tx.clone());
        Ok(Some(tx))
    }

    fn asset_balance(&mut self, asset: &[u8; 32], account: &[u8; 32]) -> Result<u128, HostError> {
        builtin::balance(&self.ov, asset, account).map_err(host_err)
    }

    fn asset_transfer(
        &mut self,
        asset: &[u8; 32],
        from: &[u8; 32],
        to: &[u8; 32],
        amount: u128,
    ) -> Result<(), HostError> {
        builtin::transfer(&mut self.ov, asset, from, to, amount).map_err(host_err)?;
        self.ov.record(
            *asset,
            ChangeKind::Payout {
                from: *from,
                to: *to,
                amount,
            },
        );
        Ok(())
    }

    fn transfer_attached(
        &mut self,
        asset: &[u8; 32],
        from: &[u8; 32],
        to: &[u8; 32],
        amount: u128,
    ) -> Result<(), HostError> {
        builtin::transfer(&mut self.ov, asset, from, to, amount).map_err(host_err)?;
        self.ov.record(
            *asset,
            ChangeKind::Funds {
                from: *from,
                to: *to,
                amount,
            },
        );
        Ok(())
    }

    fn asset_issue(
        &mut self,
        asset: &[u8; 32],
        issuer: &[u8; 32],
        meta: &AssetMetaInput,
        to: &[u8; 32],
        amount: u128,
    ) -> Result<(), HostError> {
        // The first issuance creates the asset with `mint` alone (D-121)
        let created = builtin::asset_meta(&self.ov, asset)
            .map_err(host_err)?
            .is_none();
        builtin::issue(&mut self.ov, asset, issuer, meta, to, amount).map_err(host_err)?;
        if created {
            self.ov.record(
                *asset,
                ChangeKind::Create {
                    capabilities: DEFAULT_CAPABILITIES,
                },
            );
        }
        self.ov
            .record(*asset, ChangeKind::Issue { to: *to, amount });
        Ok(())
    }

    fn asset_create(
        &mut self,
        asset: &[u8; 32],
        issuer: &[u8; 32],
        meta: &AssetMetaInput,
        capabilities: u8,
        to: &[u8; 32],
        amount: u128,
    ) -> Result<(), HostError> {
        builtin::create(&mut self.ov, asset, issuer, meta, capabilities, to, amount)
            .map_err(host_err)?;
        self.ov.record(*asset, ChangeKind::Create { capabilities });
        self.ov
            .record(*asset, ChangeKind::Issue { to: *to, amount });
        Ok(())
    }

    fn asset_set_status(
        &mut self,
        asset: &[u8; 32],
        issuer: &[u8; 32],
        account: &[u8; 32],
        flag: u8,
        on: bool,
    ) -> Result<(), HostError> {
        let kind = match flag {
            STATUS_FROZEN => Some(ChangeKind::Freeze {
                account: *account,
                frozen: on,
            }),
            STATUS_ALLOWED => Some(ChangeKind::Allow {
                account: *account,
                allowed: on,
            }),
            // `set_status` refuses any other flag
            _ => None,
        };
        builtin::set_status(&mut self.ov, asset, issuer, account, flag, on).map_err(host_err)?;
        if let Some(kind) = kind {
            self.ov.record(*asset, kind);
        }
        Ok(())
    }

    fn asset_pause(
        &mut self,
        asset: &[u8; 32],
        issuer: &[u8; 32],
        paused: bool,
    ) -> Result<(), HostError> {
        builtin::set_paused(&mut self.ov, asset, issuer, paused).map_err(host_err)?;
        self.ov.record(*asset, ChangeKind::Pause { paused });
        Ok(())
    }

    fn asset_renounce(
        &mut self,
        asset: &[u8; 32],
        issuer: &[u8; 32],
        capabilities: u8,
    ) -> Result<(), HostError> {
        // Capabilities the asset no longer has are ignored (D-121): the change names those it gives up
        let held = builtin::asset_meta(&self.ov, asset)
            .map_err(host_err)?
            .map_or(0, |m| m.capabilities);
        builtin::renounce(&mut self.ov, asset, issuer, capabilities).map_err(host_err)?;
        self.ov.record(
            *asset,
            ChangeKind::Renounce {
                capabilities: held & capabilities,
            },
        );
        Ok(())
    }

    fn asset_policy(&mut self, asset: &[u8; 32]) -> Result<Option<AssetPolicy>, HostError> {
        Ok(builtin::asset_meta(&self.ov, asset)
            .map_err(host_err)?
            .map(|m| AssetPolicy {
                capabilities: m.capabilities,
                paused: m.paused,
            }))
    }

    fn asset_status(&mut self, asset: &[u8; 32], account: &[u8; 32]) -> Result<u8, HostError> {
        builtin::asset_status(&self.ov, asset, account).map_err(host_err)
    }

    fn asset_burn(
        &mut self,
        asset: &[u8; 32],
        from: &[u8; 32],
        amount: u128,
    ) -> Result<(), HostError> {
        builtin::burn(&mut self.ov, asset, from, amount).map_err(host_err)?;
        self.ov.record(
            *asset,
            ChangeKind::Burn {
                from: *from,
                amount,
            },
        );
        Ok(())
    }

    fn code(&mut self, code_hash: &[u8; 32]) -> Result<Option<Vec<u8>>, HostError> {
        self.ov
            .get(&keys::code(code_hash))
            .map_err(HostError::Fatal)
    }

    fn create_contract(
        &mut self,
        contract: &[u8; 32],
        code_hash: &[u8; 32],
        creator: &[u8; 32],
    ) -> Result<(), HostError> {
        builtin::write(
            &mut self.ov,
            keys::contract(contract),
            &ContractMeta {
                code_hash: *code_hash,
                deployer: *creator,
            },
        )
        .map_err(host_err)
    }

    fn contract_code(&mut self, contract: &[u8; 32]) -> Result<Option<ContractCode>, HostError> {
        let meta: Option<ContractMeta> =
            builtin::read(&self.ov, &keys::contract(contract)).map_err(host_err)?;
        let Some(meta) = meta else {
            return Ok(None);
        };
        match self
            .ov
            .get(&keys::code(&meta.code_hash))
            .map_err(HostError::Fatal)?
        {
            Some(code) => Ok(Some(ContractCode {
                code_hash: meta.code_hash,
                code,
            })),
            None => Err(HostError::Fatal("contract code missing".into())),
        }
    }

    fn checkpoint(&mut self) -> Result<(), HostError> {
        self.ov.checkpoint();
        Ok(())
    }

    fn revert_checkpoint(&mut self) -> Result<(), HostError> {
        self.ov.revert_checkpoint().map_err(HostError::Fatal)
    }

    fn release_checkpoint(&mut self) -> Result<(), HostError> {
        self.ov.release_checkpoint().map_err(HostError::Fatal)
    }

    fn emit_event(&mut self, contract: &[u8; 32], data: &[u8]) -> Result<(), HostError> {
        self.ov.emit(Event {
            contract: *contract,
            data: data.to_vec(),
        });
        Ok(())
    }
}
