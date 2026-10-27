#[test]
fn workspace_contracts_are_wired() {
    assert!(!tagteam_hub::HUB_VERSION.is_empty());
    assert!(!tagteam_node::NODE_VERSION.is_empty());
}
