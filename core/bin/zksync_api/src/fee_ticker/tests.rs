use actix_web::{web, App, HttpResponse, HttpServer};

use async_trait::async_trait;
use bigdecimal::BigDecimal;
use chrono::Utc;
use futures::future::{AbortHandle, Abortable};
use futures::{channel::mpsc, executor::block_on};
use std::str::FromStr;
use std::thread::sleep;
use tokio::time::Duration;
use zksync_types::{Address, Token, TokenId, TokenKind, TokenPrice};
use zksync_utils::{
    big_decimal_to_ratio, ratio_to_big_decimal, ratio_to_scaled_u64, scaled_u64_to_ratio,
    UnsignedRatioSerializeAsDecimal,
};

use crate::fee_ticker::{
    ticker_api::{
        coingecko::{CoinGeckoTokenInfo, CoinGeckoTokenList},
        TokenPriceAPI,
    },
    validator::{cache::TokenInMemoryCache, FeeTokenValidator},
};

use super::*;
use crate::fee_ticker::ticker_info::BlocksInFutureAggregatedOperations;

const TEST_FAST_WITHDRAW_COEFF: f64 = 10.0;

#[derive(Debug, Clone)]
struct TestToken {
    id: TokenId,
    price_usd: Ratio<BigUint>,
    risk_factor: Option<Ratio<BigUint>>,
    precision: u8,
    address: Address,
}

impl TestToken {
    fn new(
        id: TokenId,
        price_usd: f64,
        risk_factor: Option<f64>,
        precision: u8,
        address: Address,
    ) -> Self {
        Self {
            id,
            price_usd: UnsignedRatioSerializeAsDecimal::deserialize_from_str_with_dot(
                &price_usd.to_string(),
            )
            .unwrap(),
            risk_factor: risk_factor.map(|risk_factor| {
                UnsignedRatioSerializeAsDecimal::deserialize_from_str_with_dot(
                    &risk_factor.to_string(),
                )
                .unwrap()
            }),
            precision,
            address,
        }
    }

    fn risk_factor(&self) -> Ratio<BigUint> {
        self.risk_factor
            .clone()
            .unwrap_or_else(|| Ratio::from_integer(1u32.into()))
    }

    fn eth() -> Self {
        Self::new(TokenId(0), 182.0, None, 18, Address::default())
    }

    fn hex() -> Self {
        Self::new(
            TokenId(1),
            1.0,
            Some(2.5),
            6,
            Address::from_str("34083bbd70d394110487feaa087da875a54624ec").unwrap(),
        )
    }

    fn cheap() -> Self {
        Self::new(TokenId(2), 1.0, Some(2.5), 6, Address::default())
    }

    fn expensive() -> Self {
        Self::new(TokenId(3), 173_134.192_3, Some(0.9), 18, Address::default())
    }

    fn zero_price() -> Self {
        Self::new(TokenId(4), 0.0, Some(0.9), 18, Address::default())
    }

    fn all_tokens() -> Vec<Self> {
        vec![
            Self::eth(),
            Self::cheap(),
            Self::expensive(),
            Self::hex(),
            Self::zero_price(),
        ]
    }
}

const SUBSIDY_CPK_PRICE_USD_SCALED: u64 = 10000000; // 10 dollars

fn get_test_ticker_config() -> TickerConfig {
    TickerConfig {
        zkp_cost_chunk_usd: UnsignedRatioSerializeAsDecimal::deserialize_from_str_with_dot("0.001")
            .unwrap(),
        gas_cost_tx: GasOperationsCost::from_constants(TEST_FAST_WITHDRAW_COEFF),
        tokens_risk_factors: TestToken::all_tokens()
            .into_iter()
            .filter_map(|t| {
                let id = t.id;
                t.risk_factor.map(|risk| (id, risk))
            })
            .collect(),
        scale_fee_coefficient: Ratio::new(BigUint::from(150u32), BigUint::from(100u32)),
        max_blocks_to_aggregate: 5,
        subsidy_cpk_price_usd: scaled_u64_to_ratio(SUBSIDY_CPK_PRICE_USD_SCALED),
    }
}

struct MockApiProvider;
#[async_trait]
impl FeeTickerAPI for MockApiProvider {
    async fn get_last_quote(&self, token: TokenLike) -> Result<TokenPrice, PriceError> {
        for test_token in TestToken::all_tokens() {
            if TokenLike::Id(test_token.id) == token {
                let token_price = TokenPrice {
                    usd_price: test_token.price_usd,
                    last_updated: Utc::now(),
                };
                return Ok(token_price);
            }
        }
        unreachable!("incorrect token input")
    }

    /// Get current gas price in ETH
    async fn get_gas_price_wei(&self) -> Result<BigUint, anyhow::Error> {
        Ok(BigUint::from(10u32).pow(7u32)) // 10 GWei
    }

    async fn get_token(&self, token: TokenLike) -> Result<Token, anyhow::Error> {
        for test_token in TestToken::all_tokens() {
            if TokenLike::Id(test_token.id) == token {
                return Ok(Token::new(
                    test_token.id,
                    test_token.address,
                    "",
                    test_token.precision,
                    TokenKind::ERC20,
                ));
            }
        }
        unreachable!("incorrect token input")
    }

    async fn keep_price_updated(self) {
        // Just do nothing
    }
}

struct MockTickerInfo {
    pub future_blocks: BlocksInFutureAggregatedOperations,
    pub remaining_chunks: Option<usize>,
}

impl Default for MockTickerInfo {
    fn default() -> Self {
        Self {
            future_blocks: BlocksInFutureAggregatedOperations {
                blocks_to_commit: 0,
                blocks_to_prove: 0,
                blocks_to_execute: 0,
            },
            remaining_chunks: None,
        }
    }
}

#[async_trait]
impl FeeTickerInfo for MockTickerInfo {
    async fn is_account_new(&mut self, _address: Address) -> bool {
        // Always false for simplicity.
        false
    }

    async fn blocks_in_future_aggregated_operations(
        &mut self,
    ) -> BlocksInFutureAggregatedOperations {
        self.future_blocks.clone()
    }

    async fn remaining_chunks_in_pending_block(&mut self) -> Option<usize> {
        self.remaining_chunks
    }
}

fn format_with_dot(num: &Ratio<BigUint>, precision: usize) -> String {
    UnsignedRatioSerializeAsDecimal::serialize_to_str_with_dot(num, precision)
}

#[derive(Debug)]
struct FakeTokenWatcher;

#[async_trait::async_trait]
impl TokenWatcher for FakeTokenWatcher {
    async fn get_token_market_volume(&mut self, _token: &Token) -> anyhow::Result<BigDecimal> {
        unreachable!()
    }
}

struct ErrorTickerApi;

#[async_trait::async_trait]
impl TokenPriceAPI for ErrorTickerApi {
    async fn get_price(&self, _token: &Token) -> Result<TokenPrice, PriceError> {
        Err(PriceError::token_not_found("Wrong token"))
    }
}

fn run_server(token_address: Address) -> (String, AbortHandle) {
    let mut url = None;
    let mut server = None;
    for i in 9000..9999 {
        let new_url = format!("127.0.0.1:{}", i);
        // Try to bind to some port, hope that 999 variants will be enough
        if let Ok(ser) = HttpServer::new(move || {
            App::new()
                .service(
                    web::resource("/api/v3/coins/DAI/market_chart").route(web::get().to(|| {
                        sleep(Duration::from_secs(100));
                        HttpResponse::MethodNotAllowed()
                    })),
                )
                .service(web::resource("/api/v3/coins/list").to(move || {
                    let mut platforms = HashMap::new();
                    platforms.insert(
                        String::from("ethereum"),
                        serde_json::Value::String(serde_json::to_string(&token_address).unwrap()),
                    );
                    HttpResponse::Ok().json(CoinGeckoTokenList(vec![CoinGeckoTokenInfo {
                        id: "dai".to_string(),
                        platforms,
                    }]))
                }))
        })
        .bind(new_url.clone())
        {
            server = Some(ser);
            url = Some(new_url);
            break;
        }
    }

    let server = server.expect("Could not bind to port from 9000 to 9999");
    let (abort_handle, abort_registration) = AbortHandle::new_pair();
    let future = Abortable::new(server.run(), abort_registration);
    tokio::spawn(future);
    let address = format!("http://{}/", &url.unwrap());
    (address, abort_handle)
}

type TestFeeTicker = FeeTicker<MockApiProvider, MockTickerInfo, FakeTokenWatcher>;

fn get_normal_and_subsidy_fee(
    ticker: &mut TestFeeTicker,
    tx_type: TxFeeTypes,
    token: TokenLike,
    address: Address,
    future_blocks: Option<BlocksInFutureAggregatedOperations>,
    remaining_chunks: Option<usize>,
) -> (Ratio<BigUint>, Ratio<BigUint>) {
    if let Some(blocks) = future_blocks {
        ticker.info.future_blocks = blocks;
    }
    ticker.info.remaining_chunks = remaining_chunks;
    let fee_in_token = block_on(ticker.get_fee_from_ticker_in_wei(tx_type, token.clone(), address))
        .expect("failed to get fee in token");

    let batched_fee_in_token =
        block_on(ticker.get_batch_from_ticker_in_wei(token.clone(), vec![(tx_type, address)]))
            .expect("failed to get batched fee for token");

    assert_eq!(
        fee_in_token.normal_fee.total_fee,
        batched_fee_in_token.normal_fee.total_fee
    );
    assert_eq!(
        fee_in_token.subsidized_fee.total_fee,
        batched_fee_in_token.subsidized_fee.total_fee
    );

    (
        Ratio::from(fee_in_token.normal_fee.total_fee),
        Ratio::from(fee_in_token.subsidized_fee.total_fee),
    )
}

fn get_token_fee_in_usd(
    ticker: &mut TestFeeTicker,
    tx_type: TxFeeTypes,
    token: TokenLike,
    address: Address,
    future_blocks: Option<BlocksInFutureAggregatedOperations>,
    remaining_chunks: Option<usize>,
) -> Ratio<BigUint> {
    let fee_in_token = get_normal_and_subsidy_fee(
        ticker,
        tx_type,
        token.clone(),
        address,
        future_blocks,
        remaining_chunks,
    )
    .0;

    let token_precision = block_on(MockApiProvider.get_token(token.clone()))
        .unwrap()
        .decimals;

    // Fee in usd
    (block_on(MockApiProvider.get_last_quote(token))
        .expect("failed to get fee in usd")
        .usd_price
        / BigUint::from(10u32).pow(u32::from(token_precision)))
        * fee_in_token
}

fn get_subsidy_token_fee_in_usd(
    ticker: &mut TestFeeTicker,
    tx_type: TxFeeTypes,
    token: TokenLike,
    address: Address,
    future_blocks: Option<BlocksInFutureAggregatedOperations>,
    remaining_chunks: Option<usize>,
) -> Ratio<BigUint> {
    let fee_in_token = get_normal_and_subsidy_fee(
        ticker,
        tx_type,
        token.clone(),
        address,
        future_blocks,
        remaining_chunks,
    )
    .1;
    let token_precision = block_on(MockApiProvider.get_token(token.clone()))
        .unwrap()
        .decimals;

    // Fee in usd
    (block_on(MockApiProvider.get_last_quote(token))
        .expect("failed to get fee in usd")
        .usd_price
        / BigUint::from(10u32).pow(u32::from(token_precision)))
        * fee_in_token
}

fn convert_to_usd(amount: Ratio<BigUint>, token: TokenLike) -> Ratio<BigUint> {
    let token_precision = block_on(MockApiProvider.get_token(token.clone()))
        .unwrap()
        .decimals;

    // Fee in usd
    (block_on(MockApiProvider.get_last_quote(token))
        .expect("failed to get fee in usd")
        .usd_price
        / BigUint::from(10u32).pow(u32::from(token_precision)))
        * amount
}

// Because of various precision errors, the USD price may differ, but no more than by 3 cents
const TOLERARED_PRICE_DIFFERENCE_SCALED: i64 = 3000000;

#[test]
fn test_ticker_subsidy() {
    let validator = FeeTokenValidator::new(
        TokenInMemoryCache::new(),
        chrono::Duration::seconds(100),
        BigDecimal::from(100),
        Default::default(),
        FakeTokenWatcher,
    );

    let config = get_test_ticker_config();
    let mut ticker = FeeTicker::new(
        MockApiProvider,
        MockTickerInfo::default(),
        mpsc::channel(1).1,
        config,
        validator,
    );

    // Only CREATE2 is subsidized
    let cpk = |cpk_type: ChangePubKeyType| {
        TxFeeTypes::ChangePubKey(ChangePubKeyFeeTypeArg::ContractsV4Version(cpk_type))
    };

    let (create2_normal_price, create2_subsidy_price) = get_normal_and_subsidy_fee(
        &mut ticker,
        cpk(ChangePubKeyType::CREATE2),
        TokenId(0).into(),
        Address::default(),
        None,
        None,
    );
    let create2_subsidy_price_usd =
        convert_to_usd(create2_subsidy_price.clone(), TokenLike::Id(TokenId(0)));

    // let create2_subsidy_price_usd = get_subsidy_token_fee_in_usd(
    //     &mut ticker,
    //     cpk_create2(),
    //     TokenId(0).into(),
    //     Address::default(),
    //     None,
    //     None
    // );
    // let create2_token_price_usd = get_token_fee_in_usd(
    //     &mut ticker,
    //     cpk_create2(),
    //     TokenId(0).into(),
    //     Address::default(),
    //     None,
    //     None
    // );
    // Due to precision-rounding, the price might differ, but it shouldn't by more than 1 cent
    assert!(
        SUBSIDY_CPK_PRICE_USD_SCALED - ratio_to_scaled_u64(create2_subsidy_price_usd.clone())
            <= TOLERARED_PRICE_DIFFERENCE_SCALED as u64
    );
    // Just to check that subsidy fee does not coincide with normal fee
    assert_ne!(create2_normal_price, create2_subsidy_price);

    // ChangePubKey (ECDSA) is not subsidized
    let (normal_cpk_onchain_price, subsidy_cpk_onchain_price) = get_normal_and_subsidy_fee(
        &mut ticker,
        cpk(ChangePubKeyType::ECDSA),
        TokenId(0).into(),
        Address::default(),
        None,
        None,
    );
    assert_eq!(normal_cpk_onchain_price, subsidy_cpk_onchain_price);

    let (normal_cpk_ecdsa_price, subsidy_cpk_ecdsa_price) = get_normal_and_subsidy_fee(
        &mut ticker,
        cpk(ChangePubKeyType::Onchain),
        TokenId(0).into(),
        Address::default(),
        None,
        None,
    );
    assert_eq!(normal_cpk_ecdsa_price, subsidy_cpk_ecdsa_price);

    // Transfer is not subsidized
    let (normal_transfer_price, subsidy_transfer_price) = get_normal_and_subsidy_fee(
        &mut ticker,
        TxFeeTypes::Transfer,
        TokenId(0).into(),
        Address::default(),
        None,
        None,
    );
    assert_eq!(normal_transfer_price, subsidy_transfer_price);
    let normal_transfer_price_usd =
        convert_to_usd(normal_transfer_price, TokenLike::Id(TokenId(0)));

    // Subsidy also works for batches
    let batch_price_token = block_on(ticker.get_batch_from_ticker_in_wei(
        TokenId(0).into(),
        vec![
            (TxFeeTypes::Transfer, Address::default()),
            (cpk(ChangePubKeyType::CREATE2), Address::default()),
            (cpk(ChangePubKeyType::CREATE2), Address::default()),
        ],
    ))
    .unwrap();
    let subsidy_batch_price_usd = convert_to_usd(
        Ratio::from(batch_price_token.subsidized_fee.total_fee),
        TokenLike::Id(TokenId(0)),
    );

    let separate_tx_price =
        normal_transfer_price_usd + &create2_subsidy_price_usd + &create2_subsidy_price_usd;

    let diff_usd = if subsidy_batch_price_usd > separate_tx_price {
        subsidy_batch_price_usd - separate_tx_price
    } else {
        separate_tx_price - subsidy_batch_price_usd
    };
    let diff_cents = ratio_to_scaled_u64(diff_usd);
    // The batch price and the actual price may differ, but no more than by a few cents
    assert!(diff_cents < TOLERARED_PRICE_DIFFERENCE_SCALED as u64);

    // The subsidy price is more-or-less same in all tokens
    let mut prices: Vec<i64> = vec![];
    let tokens: Vec<_> = TestToken::all_tokens().into_iter().take(3).collect();

    for token in tokens.clone().into_iter() {
        let price_usd = get_subsidy_token_fee_in_usd(
            &mut ticker,
            cpk(ChangePubKeyType::CREATE2),
            token.id.into(),
            Address::default(),
            None,
            None,
        );
        let price_cents = ratio_to_u64(price_usd * BigUint::from(100u64)) as i64; // Converting to i64 to easier find differences
        prices.push(price_cents);
    }
    for i in 0..=1 {
        assert!((prices[i] - prices[i + 1]).abs() <= TOLERARED_PRICE_DIFFERENCE_SCALED);
    }

    //    assert_eq!(abs(subsidy_batch_price_usd - normal_transfer_price + &create2_subsidy_price + &create2_subsidy_price);

    // Even ChangePubKeys of other types are not subsidized
    // let normal_cpk_ecdsa_price_usd = get_token_fee_in_usd(
    //     &mut ticker,
    //     TxFeeTypes::ChangePubKey(ChangePubKeyFeeTypeArg::ContractsV4Version(ChangePubKeyType::CREATE2)),
    //     TokenId(0).into(),
    //     Address::default(),
    //     None,
    //     None
    // );
    // let subsidy_cpk_ecdsa_price_usd = get_subsidy_token_fee_in_usd(
    //     &mut ticker,
    //     TxFeeTypes::ChangePubKey(ChangePubKeyFeeTypeArg::ContractsV4Version(ChangePubKeyType::CREATE2)),
    //     TokenId(0).into(),
    //     Address::default(),
    //     None,
    //     None
    // );
}

#[test]
fn test_ticker_formula() {
    let validator = FeeTokenValidator::new(
        TokenInMemoryCache::new(),
        chrono::Duration::seconds(100),
        BigDecimal::from(100),
        Default::default(),
        FakeTokenWatcher,
    );

    let config = get_test_ticker_config();
    let mut ticker = FeeTicker::new(
        MockApiProvider,
        MockTickerInfo::default(),
        mpsc::channel(1).1,
        config,
        validator,
    );

    let get_relative_diff = |a: &Ratio<BigUint>, b: &Ratio<BigUint>| -> BigDecimal {
        let max = std::cmp::max(a.clone(), b.clone());
        let min = std::cmp::min(a.clone(), b.clone());
        ratio_to_big_decimal(&((&max - &min) / min), 6)
    };

    let expected_price_of_eth_token_transfer_usd = get_token_fee_in_usd(
        &mut ticker,
        TxFeeTypes::Transfer,
        TokenId(0).into(),
        Address::default(),
        None,
        None,
    );

    let expected_price_of_eth_token_withdraw_usd = get_token_fee_in_usd(
        &mut ticker,
        TxFeeTypes::Withdraw,
        TokenId(0).into(),
        Address::default(),
        None,
        None,
    );

    // Cost of the transfer and withdraw in USD should be the same for all tokens up to +/- 3 digits
    // (mantissa len == 11)
    let threshold = BigDecimal::from_str("0.01").unwrap();
    for token in &[TestToken::eth(), TestToken::expensive()] {
        let transfer_fee = get_token_fee_in_usd(
            &mut ticker,
            TxFeeTypes::Transfer,
            token.id.into(),
            Address::default(),
            None,
            None,
        );
        let expected_fee = expected_price_of_eth_token_transfer_usd.clone() * token.risk_factor();
        let transfer_diff = get_relative_diff(&transfer_fee, &expected_fee);
        assert!(
                transfer_diff <= threshold.clone(),
                "token transfer fee is above eth fee threshold: <{:?}: {}, ETH: {}, diff: {}, threshold: {}>",
                token.id,
                format_with_dot(&transfer_fee, 6),
                format_with_dot(&expected_fee, 6),
                transfer_diff, &threshold
            );

        let withdraw_fee = get_token_fee_in_usd(
            &mut ticker,
            TxFeeTypes::Withdraw,
            token.id.into(),
            Address::default(),
            None,
            None,
        );
        let expected_fee = expected_price_of_eth_token_withdraw_usd.clone() * token.risk_factor();
        let withdraw_diff = get_relative_diff(&withdraw_fee, &expected_fee);
        assert!(
                withdraw_diff <= threshold.clone(),
                "token withdraw fee is above eth fee threshold: <{:?}: {}, ETH: {}, diff: {}, threshold: {}>",
                token.id,
                format_with_dot(&withdraw_fee, 6),
                format_with_dot(&expected_fee, 6),
                withdraw_diff, &threshold
            );

        let mut last_fast_withdraw_fee = get_token_fee_in_usd(
            &mut ticker,
            TxFeeTypes::FastWithdraw,
            token.id.into(),
            Address::default(),
            Some(BlocksInFutureAggregatedOperations {
                blocks_to_commit: 0,
                blocks_to_prove: 0,
                blocks_to_execute: 0,
            }),
            None,
        );

        for i in 1..5 {
            let future_blocks = BlocksInFutureAggregatedOperations {
                blocks_to_commit: i,
                blocks_to_prove: i,
                blocks_to_execute: i,
            };
            let fast_withdraw_fee = get_token_fee_in_usd(
                &mut ticker,
                TxFeeTypes::FastWithdraw,
                token.id.into(),
                Address::default(),
                Some(future_blocks.clone()),
                None,
            );

            let expected_price_of_eth_token_fast_withdraw_usd = get_token_fee_in_usd(
                &mut ticker,
                TxFeeTypes::FastWithdraw,
                TokenId(0).into(),
                Address::default(),
                Some(future_blocks.clone()),
                None,
            );
            let expected_fee =
                expected_price_of_eth_token_fast_withdraw_usd.clone() * token.risk_factor();
            let fast_withdraw_diff = get_relative_diff(&fast_withdraw_fee, &expected_fee);
            assert!(
                fast_withdraw_diff <= threshold.clone(),
                "token fast withdraw fee is above eth fee threshold: <{:?}: {}, ETH: {}, diff: {}, threshold: {}>",
                token.id,
                format_with_dot(&fast_withdraw_fee, 6),
                format_with_dot(&expected_fee, 6),
                fast_withdraw_diff, &threshold
            );

            assert!(
                fast_withdraw_fee > withdraw_fee,
                "Fast withdraw fee must be greater than usual withdraw fee"
            );

            assert!(
                fast_withdraw_fee < last_fast_withdraw_fee,
                "Fast withdraw should depend on number of future blocks"
            );
            last_fast_withdraw_fee = fast_withdraw_fee;
        }

        let fast_withdraw_fee_for_6_block = get_token_fee_in_usd(
            &mut ticker,
            TxFeeTypes::FastWithdraw,
            token.id.into(),
            Address::default(),
            Some(BlocksInFutureAggregatedOperations {
                blocks_to_commit: 6,
                blocks_to_prove: 6,
                blocks_to_execute: 6,
            }),
            None,
        );

        let fast_withdraw_fee_for_1_block = get_token_fee_in_usd(
            &mut ticker,
            TxFeeTypes::FastWithdraw,
            token.id.into(),
            Address::default(),
            Some(BlocksInFutureAggregatedOperations {
                blocks_to_commit: 1,
                blocks_to_prove: 1,
                blocks_to_execute: 1,
            }),
            None,
        );

        assert_eq!(
            fast_withdraw_fee_for_1_block, fast_withdraw_fee_for_6_block,
            "Fee should be the same because 5 blocks should aggregate independent"
        );

        let mut last_fast_withdraw_fee = get_token_fee_in_usd(
            &mut ticker,
            TxFeeTypes::FastWithdraw,
            token.id.into(),
            Address::default(),
            Some(BlocksInFutureAggregatedOperations {
                blocks_to_commit: 1,
                blocks_to_prove: 2,
                blocks_to_execute: 3,
            }),
            Some(50),
        );
        for i in 2..=4 {
            let fast_withdraw_fee = get_token_fee_in_usd(
                &mut ticker,
                TxFeeTypes::FastWithdraw,
                token.id.into(),
                Address::default(),
                Some(BlocksInFutureAggregatedOperations {
                    blocks_to_commit: 1,
                    blocks_to_prove: 2,
                    blocks_to_execute: 3,
                }),
                Some((50 * i) as usize),
            );
            assert!(
                fast_withdraw_fee > last_fast_withdraw_fee,
                "Fast withdraw should depend on remaining chunks"
            );
            last_fast_withdraw_fee = fast_withdraw_fee;
        }
        let not_enough_chunks_fee = get_token_fee_in_usd(
            &mut ticker,
            TxFeeTypes::FastWithdraw,
            token.id.into(),
            Address::default(),
            Some(BlocksInFutureAggregatedOperations {
                blocks_to_commit: 1,
                blocks_to_prove: 2,
                blocks_to_execute: 3,
            }),
            Some(1),
        );

        let no_pending_block_fee = get_token_fee_in_usd(
            &mut ticker,
            TxFeeTypes::FastWithdraw,
            token.id.into(),
            Address::default(),
            Some(BlocksInFutureAggregatedOperations {
                blocks_to_commit: 1,
                blocks_to_prove: 2,
                blocks_to_execute: 3,
            }),
            None,
        );

        assert_eq!(
            not_enough_chunks_fee, no_pending_block_fee,
            "Fee should be the same because we have to add one full block for this withdraw in both options"
        );
    }
}

// It's temporary solution while zero-price tokens marked as allowed for fee
#[test]
fn test_zero_price_token_fee() {
    let validator = FeeTokenValidator::new(
        TokenInMemoryCache::new(),
        chrono::Duration::seconds(100),
        BigDecimal::from(100),
        Default::default(),
        FakeTokenWatcher,
    );

    let config = get_test_ticker_config();
    let mut ticker = FeeTicker::new(
        MockApiProvider,
        MockTickerInfo::default(),
        mpsc::channel(1).1,
        config,
        validator,
    );

    let token = TestToken::zero_price();

    // If token allowed for fee and price is zero, it should return error
    block_on(ticker.get_fee_from_ticker_in_wei(
        TxFeeTypes::Transfer,
        token.id.into(),
        Address::default(),
    ))
    .unwrap_err();

    block_on(ticker.get_batch_from_ticker_in_wei(
        token.id.into(),
        vec![(TxFeeTypes::Transfer, Address::default())],
    ))
    .unwrap_err();
}

#[actix_rt::test]
#[ignore]
// It's ignore because we can't initialize coingecko in current way with block
async fn test_error_coingecko_api() {
    let token = Token {
        id: TokenId(1),
        address: Address::random(),
        symbol: String::from("DAI"),
        decimals: 18,
        kind: TokenKind::ERC20,
        is_nft: false,
    };
    let (address, handler) = run_server(token.address);
    let client = reqwest::ClientBuilder::new()
        .timeout(CONNECTION_TIMEOUT)
        .connect_timeout(CONNECTION_TIMEOUT)
        .build()
        .expect("Failed to build reqwest::Client");
    let coingecko = CoinGeckoAPI::new(client, address.parse().unwrap()).unwrap();
    let validator = FeeTokenValidator::new(
        TokenInMemoryCache::new(),
        chrono::Duration::seconds(100),
        BigDecimal::from(100),
        Default::default(),
        FakeTokenWatcher,
    );
    let connection_pool = ConnectionPool::new(Some(1));
    {
        let mut storage = connection_pool.access_storage().await.unwrap();
        storage
            .tokens_schema()
            .store_token(token.clone())
            .await
            .unwrap();
        storage
            .tokens_schema()
            .update_historical_ticker_price(
                token.id,
                TokenPrice {
                    usd_price: big_decimal_to_ratio(&BigDecimal::from(10)).unwrap(),
                    last_updated: chrono::offset::Utc::now(),
                },
            )
            .await
            .unwrap();
    }
    let ticker_api = TickerApi::new(connection_pool, coingecko);

    let config = get_test_ticker_config();
    let mut ticker = FeeTicker::new(
        ticker_api,
        MockTickerInfo::default(),
        mpsc::channel(1).1,
        config,
        validator,
    );
    for _ in 0..1000 {
        ticker
            .get_fee_from_ticker_in_wei(
                TxFeeTypes::FastWithdraw,
                token.id.into(),
                Address::default(),
            )
            .await
            .unwrap();
        ticker
            .get_token_price(token.id.into(), TokenPriceRequestType::USDForOneWei)
            .await
            .unwrap();
    }
    handler.abort();
}

#[tokio::test]
#[ignore]
async fn test_error_api() {
    let validator = FeeTokenValidator::new(
        TokenInMemoryCache::new(),
        chrono::Duration::seconds(100),
        BigDecimal::from(100),
        Default::default(),
        FakeTokenWatcher,
    );
    let connection_pool = ConnectionPool::new(Some(1));
    let second_connection_pool = connection_pool.clone();
    let ticker_api = TickerApi::new(second_connection_pool, ErrorTickerApi);
    connection_pool
        .access_storage()
        .await
        .unwrap()
        .tokens_schema()
        .update_historical_ticker_price(
            TokenId(1),
            TokenPrice {
                usd_price: big_decimal_to_ratio(&BigDecimal::from(10)).unwrap(),
                last_updated: chrono::offset::Utc::now(),
            },
        )
        .await
        .unwrap();
    let config = get_test_ticker_config();
    let mut ticker = FeeTicker::new(
        ticker_api,
        MockTickerInfo::default(),
        mpsc::channel(1).1,
        config,
        validator,
    );

    ticker
        .get_fee_from_ticker_in_wei(
            TxFeeTypes::FastWithdraw,
            TokenId(1).into(),
            Address::default(),
        )
        .await
        .unwrap();
    ticker
        .get_token_price(TokenId(1).into(), TokenPriceRequestType::USDForOneWei)
        .await
        .unwrap();
}
