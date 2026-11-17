use herdr_feishu_bridge::{api, mcp};
use serde_json::json;

#[test]
fn stable_contract_admission_preserves_supported_native_identity() {
    let modern = json!({"name":"modern","terminal":"terminal","generation":"generation",
        "execution":{"api_protocol":999,"mcp_contract":mcp::CONTRACT,"companion":"original"}});
    let original = modern.clone();
    mcp::check_activation(std::slice::from_ref(&modern), api::PROTOCOL + 1).unwrap();
    assert_eq!(modern, original);
    assert_eq!(mcp::generation_contract(&modern["execution"]), Some(1));

    let published =
        json!({"name":"published","terminal":"terminal","execution":{"api_protocol":7}});
    mcp::check_activation(std::slice::from_ref(&published), 7).unwrap();
    // Its old pre-registration /state probe still requires API 7.
    assert!(mcp::check_activation(&[published], 8).is_err());
    for execution in [
        json!({}),
        json!({"api_protocol":5}),
        json!({"api_protocol":6}),
        json!({"api_protocol":7,"mcp_contract":2}),
        json!({"api_protocol":7,"mcp_contract":null}),
    ] {
        assert!(
            mcp::check_activation(&[json!({"terminal":"terminal","execution":execution})], 7)
                .is_err()
        );
    }
    // Sleeping native recovery records do not hold an active stdio contract.
    mcp::check_activation(
        &[json!({"terminal":null,"execution":{"api_protocol":4}})],
        7,
    )
    .unwrap();
}
