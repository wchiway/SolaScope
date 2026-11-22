use super::*;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

#[tokio::test]
async fn checks_and_newer_local_versions_never_stage_or_downgrade() {
    for (installed, latest, expected, force) in [
        ("1.0.0", "2.0.0", "update_available", false),
        ("2.0.0", "2.0.0", "up_to_date", false),
        ("3.0.0", "2.0.0", "up_to_date", true),
        ("2.0.0+local", "2.0.0+release", "up_to_date", false),
    ] {
        let server = MockServer::start().await;
        let target = target().unwrap();
        let suffix = if target.contains("windows") {
            ".zip"
        } else {
            ".tar.gz"
        };
        let archive = format!("solascope-{latest}-{target}{suffix}");
        let assets: Vec<_> = [archive.as_str(), "SHA256SUMS"].iter().map(|name| serde_json::json!({
            "name": name, "state": "uploaded", "size": 1,
            "browser_download_url": format!("{}/wchiway/SolaScope/releases/download/v{latest}/{name}", server.uri()),
        })).collect();
        Mock::given(method("GET"))
            .and(path("/repos/wchiway/SolaScope/releases/latest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "tag_name": format!("v{latest}"), "draft": false, "prerelease": false,
                "published_at": "2026-11-22T00:00:00Z", "assets": assets,
            })))
            .expect(1)
            .mount(&server)
            .await;
        let options = Options {
            check: !force,
            force,
            archive: None,
            checksum: None,
        };
        let report = perform(
            &download::Source::local(&server.uri()),
            &Version::parse(installed).unwrap(),
            target,
            &options,
            Some(Path::new("does-not-exist")),
        )
        .await
        .unwrap();
        assert_eq!(report.status, expected);
        assert!(report.directory.is_none());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}
