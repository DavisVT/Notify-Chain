use soroban_sdk::Env;
use crate::{AutoShareContract, AutoShareContractClient};

#[test]
fn test_version() {
    let env = Env::default();
    let contract_id = env.register_contract(None, AutoShareContract);
    let client = AutoShareContractClient::new(&env, &contract_id);

    assert_eq!(client.version(), 1);
}
