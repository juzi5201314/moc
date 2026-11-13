use super::*;

#[tokio::test]
async fn bad_configuration_disables_plugin_but_keeps_its_declarations() {
    let temp = tempfile::tempdir().unwrap();
    install(
        temp.path(),
        "bad",
        "import {config} from 'moc'; config.register('prefix',{type:'text',default:'ok'}); export default {};",
    );
    let bridge = Arc::new(MemoryPersistence::default());
    bridge
        .config
        .lock()
        .insert("prefix".into(), serde_json::json!(42));
    let host =
        PluginHost::start_with_persistence(temp.path(), temp.path(), &settings(), &[], bridge)
            .await;
    assert!(
        host.info()[0]
            .error
            .as_ref()
            .unwrap()
            .contains("E_PLUGIN_CONFIG")
    );
    assert_eq!(host.config_declarations()[0].2[0].key, "prefix");
}
