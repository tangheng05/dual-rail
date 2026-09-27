use dual_rail_rails::card::StripeGateway;
use dual_rail_rails::khqr::BakongVerifier;
use khqr_api::{BakongClient, Environment};

#[test]
fn real_provider_clients_can_be_built() {
    dual_rail_api::install_crypto_provider();

    StripeGateway::new("sk_test_dummy").unwrap();
    BakongVerifier::new(BakongClient::new(Environment::Sandbox, "dummy"));
}
