#[tokio::test]
async fn test_tools_v2_list() {
    let tools = memoria_mcp::v2::tools::list();
    let arr = tools.as_array().unwrap();
    assert_eq!(arr.len(), 10);
    let names: Vec<&str> = arr.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"memory_v2_remember"));
    assert!(names.contains(&"memory_v2_recall"));
    assert!(names.contains(&"memory_v2_reflect"));
    println!("✅ tools_v2_list: 10 V2 tools");
}
