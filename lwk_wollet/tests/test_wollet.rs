use std::{str::FromStr, thread, time::Duration};

use elements::bitcoin::bip32::DerivationPath;
use elements::{
    hashes::Hash, pset::PartiallySignedTransaction, Address, AssetId, ContractHash, OutPoint, Txid,
};
use elements_miniscript::DescriptorPublicKey;
use elements_miniscript::ForEachKey;
use lwk_common::Signer;
use lwk_signer::AnySigner;
use lwk_signer::SwSigner;
use lwk_test_util::{
    add_checksum, assert_fee_rate, compute_fee_rate, n_issuances, n_reissuances, pset_rt,
    TestElectrumServer, TEST_DESCRIPTOR,
};
use lwk_test_util::{generate_mnemonic, generate_slip77};
use lwk_wollet::ElementsNetwork;
use lwk_wollet::Tip;
use lwk_wollet::{
    AddressResult, BlockchainBackend, Contract, ElectrumUrl, UnvalidatedRecipient, WalletTx,
    Wollet, WolletDescriptor,
};
use tempfile::TempDir;

use crate::{full_scan_with_electrum_client, ElectrumClient, WolletTxBuilder};

pub struct TestWollet {
    pub wollet: Wollet,
    pub electrum_url: ElectrumUrl,
    db_root_dir: TempDir,
}

impl TestWollet {
    pub fn new(electrs_url: &str, desc: &str) -> Self {
        let db_root_dir = TempDir::new().unwrap();
        Self::with_temp_dir(electrs_url, desc, db_root_dir)
    }

    pub fn with_test_desc(electrs_url: &str) -> Self {
        Self::new(electrs_url, TEST_DESCRIPTOR)
    }

    pub fn with_temp_dir(electrs_url: &str, desc: &str, db_root_dir: TempDir) -> Self {
        let tls = false;
        let validate_domain = false;

        let network = ElementsNetwork::default_regtest();
        let descriptor = add_checksum(desc);

        let desc: WolletDescriptor = descriptor.parse().unwrap();
        let mut wollet = Wollet::with_fs_persist(network, desc, &db_root_dir).unwrap();

        let electrum_url = ElectrumUrl::new(electrs_url, tls, validate_domain);

        let mut electrum_client: ElectrumClient = ElectrumClient::new(&electrum_url).unwrap();
        full_scan_with_electrum_client(&mut wollet, &mut electrum_client).unwrap();

        let mut i = 120;
        let tip = loop {
            assert!(i > 0, "1 minute without updates");
            i -= 1;
            let height = electrum_client.tip().unwrap().height;
            if height >= 101 {
                break height;
            } else {
                thread::sleep(Duration::from_millis(500));
            }
        };
        full_scan_with_electrum_client(&mut wollet, &mut electrum_client).unwrap();

        assert!(tip >= 101);

        Self {
            wollet,
            electrum_url,
            db_root_dir,
        }
    }

    pub fn tx_builder(&self) -> WolletTxBuilder {
        self.wollet.tx_builder()
    }

    pub fn db_root_dir(self) -> TempDir {
        self.db_root_dir
    }

    pub fn policy_asset(&self) -> AssetId {
        self.wollet.policy_asset()
    }

    pub fn tip(&self) -> Tip {
        self.wollet.tip()
    }

    pub fn sync(&mut self) {
        let mut electrum_client: ElectrumClient = ElectrumClient::new(&self.electrum_url).unwrap();
        full_scan_with_electrum_client(&mut self.wollet, &mut electrum_client).unwrap();
    }

    pub fn address(&self) -> Address {
        self.wollet.address(None).unwrap().address().clone()
    }

    pub fn address_result(&self, last_unused: Option<u32>) -> AddressResult {
        self.wollet.address(last_unused).unwrap()
    }

    /// Wait until tx appears in tx list (max 1 min)
    fn wait_for_tx(&mut self, txid: &Txid) {
        let mut electrum_client: ElectrumClient = ElectrumClient::new(&self.electrum_url).unwrap();
        for _ in 0..120 {
            full_scan_with_electrum_client(&mut self.wollet, &mut electrum_client).unwrap();
            let list = self.wollet.transactions().unwrap();
            if list.iter().any(|e| &e.txid == txid) {
                return;
            }
            thread::sleep(Duration::from_millis(500));
        }
        panic!("Wallet does not have {} in its list", txid);
    }

    /// asset balance in satoshi
    pub fn balance(&mut self, asset: &AssetId) -> u64 {
        let balance = self.wollet.balance().unwrap();
        *balance.get(asset).unwrap_or(&0u64)
    }

    fn balance_btc(&mut self) -> u64 {
        self.balance(&self.wollet.policy_asset())
    }

    fn get_tx(&mut self, txid: &Txid) -> WalletTx {
        self.wollet.transaction(txid).unwrap().unwrap()
    }

    pub fn fund(
        &mut self,
        server: &TestElectrumServer,
        satoshi: u64,
        address: Option<Address>,
        asset: Option<AssetId>,
    ) {
        let utxos_before = self.wollet.utxos().unwrap().len();
        let balance_before = self.balance(&asset.unwrap_or(self.policy_asset()));

        let address = address.unwrap_or_else(|| self.address());
        let txid = server.node_sendtoaddress(&address, satoshi, asset);
        self.wait_for_tx(&txid);
        let tx = self.get_tx(&txid);
        // We only received, all balances are positive
        assert!(tx.balance.values().all(|v| *v > 0));
        assert_eq!(&tx.type_, "incoming");
        let wallet_txid = tx.tx.txid();
        assert_eq!(txid, wallet_txid);
        assert_eq!(tx.inputs.iter().filter(|o| o.is_some()).count(), 0);
        assert_eq!(tx.outputs.iter().filter(|o| o.is_some()).count(), 1);

        let utxos_after = self.wollet.utxos().unwrap().len();
        let balance_after = self.balance(&asset.unwrap_or(self.policy_asset()));
        assert_eq!(utxos_after, utxos_before + 1);
        assert_eq!(balance_before + satoshi, balance_after);
    }

    pub fn fund_btc(&mut self, server: &TestElectrumServer) {
        self.fund(server, 1_000_000, Some(self.address()), None);
    }

    pub fn fund_asset(&mut self, server: &TestElectrumServer) -> AssetId {
        let satoshi = 10_000;
        let asset = server.node_issueasset(satoshi);
        self.fund(server, satoshi, Some(self.address()), Some(asset));
        asset
    }

    /// Send 10_000 satoshi to self with default fee rate.
    ///
    /// To specify a custom fee rate pass Some in `fee_rate`
    /// To specify an external recipient specify the `to` parameter
    pub fn send_btc(
        &mut self,
        signers: &[&AnySigner],
        fee_rate: Option<f32>,
        external: Option<(Address, u64)>,
    ) {
        let balance_before = self.balance_btc();

        let recipient = external.clone().unwrap_or((self.address(), 10_000));

        let mut pset = self
            .tx_builder()
            .add_lbtc_recipient(&recipient.0, recipient.1)
            .unwrap()
            .fee_rate(fee_rate)
            .finish()
            .unwrap();
        pset = pset_rt(&pset);

        let details = self.wollet.get_details(&pset).unwrap();
        let fee = details.balance.fee as i64;
        assert!(fee > 0);
        let balance = match &external {
            Some((_a, v)) => -fee - *v as i64,
            None => -fee,
        };
        assert_eq!(
            *details.balance.balances.get(&self.policy_asset()).unwrap(),
            balance
        );
        assert_eq!(n_issuances(&details), 0);
        assert_eq!(n_reissuances(&details), 0);

        for signer in signers {
            self.sign(signer, &mut pset);
        }
        assert_fee_rate(compute_fee_rate(&pset), fee_rate);
        let txid = self.send(&mut pset);
        let balance_after = self.balance_btc();
        assert!(balance_before > balance_after);
        let tx = self.get_tx(&txid);
        // We only sent, so all balances are negative
        assert!(tx.balance.values().all(|v| *v < 0));
        assert_eq!(&tx.type_, "outgoing");
        assert_eq!(tx.fee, fee as u64);
        assert!(tx.inputs.iter().filter(|o| o.is_some()).count() > 0);
        assert!(tx.outputs.iter().filter(|o| o.is_some()).count() > 0);

        self.wollet.descriptor().descriptor.for_each_key(|k| {
            if let DescriptorPublicKey::XPub(x) = k {
                if let Some(origin) = &x.origin {
                    assert_eq!(pset.global.xpub.get(&x.xkey).unwrap(), origin);
                }
            }
            true
        });
    }

    /// Send all L-BTC
    pub fn send_all_btc(
        &mut self,
        signers: &[&AnySigner],
        fee_rate: Option<f32>,
        address: Address,
    ) {
        let balance_before = self.balance_btc();

        let mut pset = self
            .tx_builder()
            .drain_lbtc_wallet()
            .drain_lbtc_to(address)
            .fee_rate(fee_rate)
            .finish()
            .unwrap();

        let details = self.wollet.get_details(&pset).unwrap();
        let fee = details.balance.fee as i64;
        assert!(fee > 0);
        assert_eq!(
            *details.balance.balances.get(&self.policy_asset()).unwrap(),
            -(balance_before as i64)
        );

        for signer in signers {
            self.sign(signer, &mut pset);
        }
        self.send(&mut pset);
        let balance_after = self.balance_btc();
        assert_eq!(balance_after, 0);
    }

    pub fn send_asset(
        &mut self,
        signers: &[&AnySigner],
        address: &Address,
        asset: &AssetId,
        fee_rate: Option<f32>,
    ) {
        let balance_before = self.balance(asset);
        let satoshi: u64 = 10;
        let mut pset = self
            .tx_builder()
            .add_recipient(address, satoshi, *asset)
            .unwrap()
            .fee_rate(fee_rate)
            .finish()
            .unwrap();

        pset = pset_rt(&pset);

        let details = self.wollet.get_details(&pset).unwrap();
        let fee = details.balance.fee as i64;
        assert!(fee > 0);
        assert_eq!(
            *details.balance.balances.get(&self.policy_asset()).unwrap(),
            -fee
        );
        assert_eq!(
            *details.balance.balances.get(asset).unwrap(),
            -(satoshi as i64)
        );
        assert_eq!(n_issuances(&details), 0);
        assert_eq!(n_reissuances(&details), 0);

        for signer in signers {
            self.sign(signer, &mut pset);
        }
        assert_fee_rate(compute_fee_rate(&pset), fee_rate);
        self.send(&mut pset);
        let balance_after = self.balance(asset);
        assert!(balance_before > balance_after);
    }

    pub fn send_many(
        &mut self,
        signers: &[&AnySigner],
        addr1: &Address,
        asset1: &AssetId,
        addr2: &Address,
        asset2: &AssetId,
        fee_rate: Option<f32>,
    ) {
        let balance1_before = self.balance(asset1);
        let balance2_before = self.balance(asset2);
        let addr1 = addr1.to_string();
        let addr2 = addr2.to_string();
        let ass1 = asset1.to_string();
        let ass2 = asset2.to_string();
        let addressees: Vec<UnvalidatedRecipient> = vec![
            UnvalidatedRecipient {
                satoshi: 1_000,
                address: addr1,
                asset: ass1,
            },
            UnvalidatedRecipient {
                satoshi: 2_000,
                address: addr2,
                asset: ass2,
            },
        ];

        let mut pset = self
            .tx_builder()
            .set_unvalidated_recipients(&addressees)
            .unwrap()
            .fee_rate(fee_rate)
            .finish()
            .unwrap();

        pset = pset_rt(&pset);

        let details = self.wollet.get_details(&pset).unwrap();
        let fee = details.balance.fee as i64;
        assert!(fee > 0);
        // Checking the balance here has a bit too many cases:
        // asset1,2 are btc, asset1,2 are equal, addr1,2 belong to the wallet
        // Skipping the checks here
        assert_eq!(n_issuances(&details), 0);
        assert_eq!(n_reissuances(&details), 0);

        for signer in signers {
            self.sign(signer, &mut pset);
        }
        assert_fee_rate(compute_fee_rate(&pset), fee_rate);
        self.send(&mut pset);
        let balance1_after = self.balance(asset1);
        let balance2_after = self.balance(asset2);
        assert!(balance1_before > balance1_after);
        assert!(balance2_before > balance2_after);
    }

    pub fn issueasset(
        &mut self,
        signers: &[&AnySigner],
        satoshi_asset: u64,
        satoshi_token: u64,
        contract: Option<&str>,
        fee_rate: Option<f32>,
    ) -> (AssetId, AssetId) {
        let balance_before = self.balance_btc();
        let contract = contract.map(|c| Contract::from_str(c).unwrap());
        let contract_hash = contract
            .as_ref()
            .map(|c| c.contract_hash().unwrap())
            .unwrap_or_else(|| ContractHash::from_slice(&[0u8; 32]).expect("static"));
        let mut pset = self
            .tx_builder()
            .issue_asset(satoshi_asset, None, satoshi_token, None, contract)
            .unwrap()
            .fee_rate(fee_rate)
            .finish()
            .unwrap();
        pset = pset_rt(&pset);

        let issuance_input = &pset.inputs()[0].clone();
        let (asset, token) = issuance_input.issuance_ids();

        let details = self.wollet.get_details(&pset).unwrap();
        assert_eq!(n_issuances(&details), 1);
        assert_eq!(n_reissuances(&details), 0);
        let issuance = &details.issuances[0];
        assert_eq!(asset, issuance.asset().unwrap());
        assert_eq!(token, issuance.token().unwrap());
        assert_eq!(satoshi_asset, issuance.asset_satoshi().unwrap());
        assert_eq!(satoshi_token, issuance.token_satoshi().unwrap());
        let fee = details.balance.fee as i64;
        assert!(fee > 0);
        assert_eq!(
            *details.balance.balances.get(&self.policy_asset()).unwrap(),
            -fee
        );
        assert_eq!(
            *details.balance.balances.get(&asset).unwrap(),
            satoshi_asset as i64
        );
        assert_eq!(
            *details.balance.balances.get(&token).unwrap_or(&0),
            satoshi_token as i64
        );

        for signer in signers {
            self.sign(signer, &mut pset);
        }
        assert_fee_rate(compute_fee_rate(&pset), fee_rate);
        let txid = self.send(&mut pset);
        let tx = self.get_tx(&txid);
        assert_eq!(&tx.type_, "issuance");

        assert_eq!(self.balance(&asset), satoshi_asset);
        assert_eq!(self.balance(&token), satoshi_token);
        let balance_after = self.balance_btc();
        assert!(balance_before > balance_after);

        let issuance = self.wollet.issuance(&asset).unwrap();
        assert_eq!(issuance.vin, 0);
        assert!(!issuance.is_reissuance);
        assert_eq!(issuance.asset_amount, Some(satoshi_asset));
        assert_eq!(issuance.token_amount, Some(satoshi_token));

        let prevout = OutPoint::new(
            issuance_input.previous_txid,
            issuance_input.previous_output_index,
        );
        assert_eq!(asset, AssetId::new_issuance(prevout, contract_hash));

        (asset, token)
    }

    pub fn reissueasset(
        &mut self,
        signers: &[&AnySigner],
        satoshi_asset: u64,
        asset: &AssetId,
        fee_rate: Option<f32>,
    ) {
        let issuance = self.wollet.issuance(asset).unwrap();
        let balance_btc_before = self.balance_btc();
        let balance_asset_before = self.balance(asset);
        let balance_token_before = self.balance(&issuance.token);
        let mut pset = self
            .tx_builder()
            .reissue_asset(*asset, satoshi_asset, None, None)
            .unwrap()
            .fee_rate(fee_rate)
            .finish()
            .unwrap();
        pset = pset_rt(&pset);

        let details = self.wollet.get_details(&pset).unwrap();
        assert_eq!(n_issuances(&details), 0);
        assert_eq!(n_reissuances(&details), 1);
        let reissuance = details
            .issuances
            .iter()
            .find(|e| e.is_reissuance())
            .unwrap();
        assert_eq!(asset, &reissuance.asset().unwrap());
        assert_eq!(issuance.token, reissuance.token().unwrap());
        assert_eq!(satoshi_asset, reissuance.asset_satoshi().unwrap());
        assert!(reissuance.token_satoshi().is_none());
        let fee = details.balance.fee as i64;
        assert!(fee > 0);
        assert_eq!(
            *details.balance.balances.get(&self.policy_asset()).unwrap(),
            -fee
        );
        assert_eq!(
            *details.balance.balances.get(asset).unwrap(),
            satoshi_asset as i64
        );
        assert_eq!(
            *details.balance.balances.get(&issuance.token).unwrap(),
            0i64
        );

        for signer in signers {
            self.sign(signer, &mut pset);
        }
        assert_fee_rate(compute_fee_rate(&pset), fee_rate);
        let txid = self.send(&mut pset);
        let tx = self.get_tx(&txid);
        assert_eq!(&tx.type_, "reissuance");

        assert_eq!(self.balance(asset), balance_asset_before + satoshi_asset);
        assert_eq!(self.balance(&issuance.token), balance_token_before);
        assert!(self.balance_btc() < balance_btc_before);

        let issuances = self.wollet.issuances().unwrap();
        assert!(issuances.len() > 1);
        let reissuance = issuances.iter().find(|e| e.txid == txid).unwrap();
        assert!(reissuance.is_reissuance);
        assert_eq!(reissuance.asset_amount, Some(satoshi_asset));
        assert!(reissuance.token_amount.is_none());
    }

    pub fn burnasset(
        &mut self,
        signers: &[&AnySigner],
        satoshi_asset: u64,
        asset: &AssetId,
        fee_rate: Option<f32>,
    ) {
        let balance_btc_before = self.balance_btc();
        let balance_asset_before = self.balance(asset);
        let mut pset = self
            .tx_builder()
            .add_burn(satoshi_asset, *asset)
            .unwrap()
            .fee_rate(fee_rate)
            .finish()
            .unwrap();
        pset = pset_rt(&pset);

        let details = self.wollet.get_details(&pset).unwrap();
        let fee = details.balance.fee as i64;
        assert!(fee > 0);
        let btc = self.policy_asset();
        let (expected_asset, expected_btc) = if asset == &btc {
            (0, -(fee + satoshi_asset as i64))
        } else {
            (-(satoshi_asset as i64), -fee)
        };
        assert_eq!(*details.balance.balances.get(&btc).unwrap(), expected_btc);
        assert_eq!(
            *details.balance.balances.get(asset).unwrap_or(&0),
            expected_asset
        );
        assert_eq!(n_issuances(&details), 0);
        assert_eq!(n_reissuances(&details), 0);

        for signer in signers {
            self.sign(signer, &mut pset);
        }
        assert_fee_rate(compute_fee_rate(&pset), fee_rate);
        let txid = self.send(&mut pset);
        let tx = self.get_tx(&txid);
        assert_eq!(&tx.type_, "burn");

        assert_eq!(self.balance(asset), balance_asset_before - satoshi_asset);
        assert!(self.balance_btc() < balance_btc_before);
    }

    pub fn sign<S: Signer>(&self, signer: &S, pset: &mut PartiallySignedTransaction) {
        *pset = pset_rt(pset);
        let sigs_added_or_overwritten = signer.sign(pset).unwrap();
        assert!(sigs_added_or_overwritten > 0);
    }

    pub fn send(&mut self, pset: &mut PartiallySignedTransaction) -> Txid {
        *pset = pset_rt(pset);
        let tx = self.wollet.finalize(pset).unwrap();
        let electrum_client = ElectrumClient::new(&self.electrum_url).unwrap();
        let txid = electrum_client.broadcast(&tx).unwrap();
        self.wait_for_tx(&txid);
        txid
    }

    pub fn check_persistence(wollet: TestWollet) {
        let descriptor = wollet.wollet.descriptor().to_string();
        let expected_updates = wollet.wollet.updates().unwrap();
        let expected = wollet.wollet.balance().unwrap();
        let db_root_dir = wollet.db_root_dir();
        let network = ElementsNetwork::default_regtest();

        for _ in 0..2 {
            let wollet =
                Wollet::with_fs_persist(network, descriptor.parse().unwrap(), &db_root_dir)
                    .unwrap();

            let balance = wollet.balance().unwrap();
            assert_eq!(expected, balance);
            assert_eq!(expected_updates, wollet.updates().unwrap());
        }
    }

    pub fn wait_height(&mut self, height: u32) {
        let mut electrum_client: ElectrumClient = ElectrumClient::new(&self.electrum_url).unwrap();
        for _ in 0..120 {
            full_scan_with_electrum_client(&mut self.wollet, &mut electrum_client).unwrap();
            if self.wollet.tip().height() == height {
                return;
            }
            thread::sleep(Duration::from_millis(500));
        }
        panic!("Wait for height {height} failed");
    }

    pub fn make_external(&mut self, utxo: &lwk_wollet::WalletTxOut) -> lwk_wollet::ExternalUtxo {
        let tx = self.get_tx(&utxo.outpoint.txid).tx;
        let txout = tx.output.get(utxo.outpoint.vout as usize).unwrap().clone();
        lwk_wollet::ExternalUtxo {
            outpoint: utxo.outpoint,
            txout,
            unblinded: utxo.unblinded,
            max_weight_to_satisfy: self.wollet.max_weight_to_satisfy(),
        }
    }
}

pub fn generate_signer() -> SwSigner {
    let mnemonic = generate_mnemonic();
    SwSigner::new(&mnemonic, false).unwrap()
}

pub fn multisig_desc(signers: &[&AnySigner], threshold: usize) -> String {
    assert!(threshold <= signers.len());
    let xpubs = signers
        .iter()
        .map(|s| {
            let fingerprint = s.fingerprint().unwrap();
            let path_str = "/84h/0h/0h";
            let path = DerivationPath::from_str(&format!("m{path_str}")).unwrap();
            let xpub = s.derive_xpub(&path).unwrap();
            format!("[{fingerprint}{path_str}]{xpub}/<0;1>/*",)
        })
        .collect::<Vec<_>>()
        .join(",");
    let slip77 = generate_slip77();
    format!("ct(slip77({slip77}),elwsh(multi({threshold},{xpubs})))")
}
