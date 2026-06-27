use anyhow::{Context, Result, anyhow, bail};
use reqwest::{
    StatusCode,
    blocking::{Client as HttpClient, RequestBuilder, Response},
    header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, WWW_AUTHENTICATE},
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::hex::hex_lower;

use super::{
    auth::{DockerConfigAuthStore, RegistryAuth},
    reference::{OciFeatureRef, validate_oci_digest},
};

const DOCKER_HUB_CANONICAL_HOST: &str = "docker.io";
const DOCKER_HUB_REGISTRY_HOST: &str = "registry-1.docker.io";
const DOCKER_HUB_INDEX_HOST: &str = "index.docker.io";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OciManifestResponse {
    pub(crate) digest: String,
    pub(crate) layers: Vec<OciLayerDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OciLayerDescriptor {
    pub(crate) digest: String,
    pub(crate) media_type: String,
    pub(crate) size: u64,
}

pub(crate) trait OciRegistryClient {
    fn fetch_manifest(&self, reference: &OciFeatureRef) -> Result<OciManifestResponse>;

    fn fetch_blob(&self, reference: &OciFeatureRef, digest: &str) -> Result<Vec<u8>>;
}

#[derive(Debug, Clone)]
pub(crate) struct HttpOciRegistryClient {
    client: HttpClient,
    auth: DockerConfigAuthStore,
}

impl HttpOciRegistryClient {
    pub(crate) fn from_docker_config() -> Result<Self> {
        Ok(Self {
            client: HttpClient::new(),
            auth: DockerConfigAuthStore::from_default_config()?,
        })
    }
}

impl OciRegistryClient for HttpOciRegistryClient {
    fn fetch_manifest(&self, reference: &OciFeatureRef) -> Result<OciManifestResponse> {
        let selector = reference
            .digest
            .as_ref()
            .or(reference.tag.as_ref())
            .ok_or_else(|| {
                anyhow!(
                    "Feature ref must include a tag or digest before registry fetch: {}",
                    reference.original
                )
            })?;
        let url = registry_url(reference, &format!("manifests/{selector}"));
        let response = self
            .send_registry_request(
                reference,
                self.client.get(&url).header(
                    ACCEPT,
                    HeaderValue::from_static(
                        "application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json",
                    ),
                ),
            )
            .with_context(|| {
                format!(
                    "Failed to fetch OCI manifest for feature {} ({selector})",
                    reference.original
                )
            })?;
        let digest = response
            .headers()
            .get("Docker-Content-Digest")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response
            .bytes()
            .with_context(|| {
                format!(
                    "Failed to read OCI manifest response for feature {}",
                    reference.original
                )
            })?
            .to_vec();

        parse_registry_manifest_response_body(
            &reference.original,
            reference.digest.as_deref(),
            digest.as_deref(),
            &body,
        )
    }

    fn fetch_blob(&self, reference: &OciFeatureRef, digest: &str) -> Result<Vec<u8>> {
        validate_oci_digest(digest).with_context(|| {
            format!(
                "Invalid OCI blob digest for feature {}: {digest}",
                reference.original
            )
        })?;
        let url = registry_url(reference, &format!("blobs/{digest}"));
        let response = self
            .send_registry_request(reference, self.client.get(&url))
            .with_context(|| {
                format!(
                    "Failed to fetch OCI blob for feature {} ({digest})",
                    reference.original
                )
            })?;

        Ok(response
            .bytes()
            .with_context(|| {
                format!(
                    "Failed to read OCI blob response for feature {} ({digest})",
                    reference.original
                )
            })?
            .to_vec())
    }
}

impl HttpOciRegistryClient {
    fn send_registry_request(
        &self,
        reference: &OciFeatureRef,
        request: RequestBuilder,
    ) -> Result<Response> {
        let request = self.apply_registry_auth(reference, request)?;
        let response = request.send().with_context(|| {
            format!(
                "Failed to send OCI registry request for feature {}",
                reference.original
            )
        })?;

        if response.status() != StatusCode::UNAUTHORIZED {
            return response.error_for_status().with_context(|| {
                format!(
                    "OCI registry returned an error for feature {}",
                    reference.original
                )
            });
        }

        let Some(challenge) = bearer_challenge(response.headers()) else {
            return response.error_for_status().with_context(|| {
                format!(
                    "OCI registry authentication failed for feature {}",
                    reference.original
                )
            });
        };
        let token = self.fetch_bearer_token(reference, &challenge)?;
        let response = Self::apply_bearer_auth(
            reference,
            self.client
                .get(response.url().clone())
                .header(ACCEPT, registry_accept_header()),
            &token,
        )
        .send()
        .with_context(|| {
            format!(
                "Failed to retry OCI registry request for feature {}",
                reference.original
            )
        })?;

        response.error_for_status().with_context(|| {
            format!(
                "OCI registry returned an error for feature {}",
                reference.original
            )
        })
    }

    fn fetch_bearer_token(
        &self,
        reference: &OciFeatureRef,
        challenge: &BearerChallenge,
    ) -> Result<String> {
        let mut request = self.client.get(&challenge.realm);
        if let Some(service) = &challenge.service {
            request = request.query(&[("service", service)]);
        }
        if let Some(scope) = challenge
            .scope
            .clone()
            .or_else(|| Some(format!("repository:{}:pull", reference.repository_path())))
        {
            request = request.query(&[("scope", &scope)]);
        }
        request = self.apply_registry_auth(reference, request)?;
        let response = request.send().with_context(|| {
            format!(
                "Failed to request OCI registry token for feature {}",
                reference.original
            )
        })?;
        let token: BearerTokenResponse = response
            .error_for_status()
            .with_context(|| {
                format!(
                    "OCI registry token service returned an error for feature {}",
                    reference.original
                )
            })?
            .json()
            .with_context(|| {
                format!(
                    "Failed to parse OCI registry token response for feature {}",
                    reference.original
                )
            })?;

        token.token.or(token.access_token).ok_or_else(|| {
            anyhow!(
                "OCI registry token response did not include a token for feature {}",
                reference.original
            )
        })
    }

    fn apply_registry_auth(
        &self,
        reference: &OciFeatureRef,
        request: RequestBuilder,
    ) -> Result<RequestBuilder> {
        Ok(match self.auth.get(&reference.registry)? {
            Some(RegistryAuth::Basic { username, password }) => {
                request.basic_auth(username, Some(password))
            }
            Some(RegistryAuth::Bearer(token)) => {
                Self::apply_bearer_auth(reference, request, &token)
            }
            None => request,
        })
    }

    fn apply_bearer_auth(
        _reference: &OciFeatureRef,
        request: RequestBuilder,
        token: &str,
    ) -> RequestBuilder {
        request.header(AUTHORIZATION, format!("Bearer {token}"))
    }
}

pub(super) fn registry_url(reference: &OciFeatureRef, suffix: &str) -> String {
    format!(
        "https://{}/v2/{}/{}",
        registry_endpoint_host(&reference.registry),
        reference.repository_path(),
        suffix
    )
}

fn registry_endpoint_host(registry: &str) -> &str {
    if matches!(
        registry,
        DOCKER_HUB_CANONICAL_HOST | DOCKER_HUB_INDEX_HOST | DOCKER_HUB_REGISTRY_HOST
    ) {
        DOCKER_HUB_REGISTRY_HOST
    } else {
        registry
    }
}

const fn registry_accept_header() -> HeaderValue {
    HeaderValue::from_static(
        "application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json, application/octet-stream, */*",
    )
}

pub(super) fn parse_registry_manifest_response_body(
    reference: &str,
    requested_digest: Option<&str>,
    header_digest: Option<&str>,
    body: &[u8],
) -> Result<OciManifestResponse> {
    let requested_digest = requested_digest
        .map(|digest| {
            validate_oci_digest(digest)
                .map(|()| digest)
                .with_context(|| format!("Invalid requested OCI manifest digest: {digest}"))
        })
        .transpose()?;
    let header_digest = header_digest
        .map(|digest| {
            validate_oci_digest(digest)
                .map(|()| digest)
                .with_context(|| format!("Invalid Docker-Content-Digest header: {digest}"))
        })
        .transpose()?;
    let actual_digest = format!("sha256:{}", hex_lower(&Sha256::digest(body)));

    if let Some(header_digest) = header_digest
        && header_digest != actual_digest
    {
        bail!(
            "OCI manifest digest mismatch for {reference}: Docker-Content-Digest is {header_digest}, body is {actual_digest}"
        );
    }
    if let Some(requested_digest) = requested_digest
        && requested_digest != actual_digest
    {
        bail!(
            "OCI manifest digest mismatch for {reference}: expected {requested_digest}, got {actual_digest}"
        );
    }

    let digest = header_digest
        .or(requested_digest)
        .unwrap_or(&actual_digest)
        .to_owned();
    let manifest: RegistryManifest = serde_json::from_slice(body).with_context(|| {
        format!("Failed to parse OCI manifest for feature {reference} ({digest})")
    })?;
    let layers = manifest
        .layers
        .into_iter()
        .map(|layer| {
            validate_oci_digest(&layer.digest).with_context(|| {
                format!(
                    "Invalid OCI manifest layer digest for feature {reference}: {}",
                    layer.digest
                )
            })?;
            Ok(OciLayerDescriptor {
                digest: layer.digest,
                media_type: layer.media_type,
                size: layer.size.unwrap_or_default(),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(OciManifestResponse { digest, layers })
}

fn bearer_challenge(headers: &HeaderMap) -> Option<BearerChallenge> {
    let value = headers.get(WWW_AUTHENTICATE)?.to_str().ok()?;
    parse_bearer_challenge(value)
}

pub(super) fn parse_bearer_challenge(value: &str) -> Option<BearerChallenge> {
    let value = value.trim_start();
    let scheme_end = value.find(char::is_whitespace)?;
    let (scheme, parameters) = value.split_at(scheme_end);
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let parameters = parameters.trim_start();
    let mut challenge = BearerChallenge::default();
    for entry in split_auth_parameters(parameters) {
        let Some((key, value)) = entry.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"').to_owned();
        let key = key.trim();
        if key.eq_ignore_ascii_case("realm") {
            challenge.realm = value;
        } else if key.eq_ignore_ascii_case("service") {
            challenge.service = Some(value);
        } else if key.eq_ignore_ascii_case("scope") {
            challenge.scope = Some(value);
        }
    }
    if challenge.realm.is_empty() {
        None
    } else {
        Some(challenge)
    }
}

fn split_auth_parameters(value: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut in_quotes = false;
    let mut start = 0;
    for (index, ch) in value.char_indices() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                let (entry, _) = value.split_at(index);
                parts.push(entry.split_at(start).1.trim());
                start = index + ','.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(value.split_at(start).1.trim());
    parts
}

#[derive(Debug, Deserialize)]
struct RegistryManifest {
    #[serde(default)]
    layers: Vec<RegistryDescriptor>,
}

#[derive(Debug, Deserialize)]
struct RegistryDescriptor {
    #[serde(rename = "mediaType", default)]
    media_type: String,
    digest: String,
    size: Option<u64>,
}

#[derive(Debug, Default)]
pub(super) struct BearerChallenge {
    pub(super) realm: String,
    pub(super) service: Option<String>,
    pub(super) scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BearerTokenResponse {
    token: Option<String>,
    access_token: Option<String>,
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::devcontainer::features::reference::parse_oci_feature_ref;

    const MANIFEST_DIGEST_HEX: &str =
        "1111111111111111111111111111111111111111111111111111111111111111";

    #[test]
    fn manifest_response_rejects_body_digest_mismatch_for_digest_reference() {
        let requested_digest = sha256_digest(MANIFEST_DIGEST_HEX);
        let body = br#"{"layers":[]}"#;

        let error = parse_registry_manifest_response_body(
            "ghcr.io/example/features/tool",
            Some(&requested_digest),
            Some(&requested_digest),
            body,
        )
        .unwrap_err();

        assert!(error.to_string().contains("digest mismatch"), "{error:#}");
        assert!(error.to_string().contains(&requested_digest), "{error:#}");
    }

    #[test]
    fn manifest_response_rejects_header_digest_mismatch() {
        let body = br#"{"layers":[]}"#;
        let actual_digest = sha256_digest(&hex_lower(&Sha256::digest(body)));
        let header_digest = sha256_digest(MANIFEST_DIGEST_HEX);

        let error = parse_registry_manifest_response_body(
            "ghcr.io/example/features/tool:1",
            None,
            Some(&header_digest),
            body,
        )
        .unwrap_err();

        assert!(error.to_string().contains("digest mismatch"), "{error:#}");
        assert!(error.to_string().contains(&actual_digest), "{error:#}");
    }

    #[test]
    fn manifest_response_uses_body_digest_when_header_is_missing_for_tag_reference() {
        let body = br#"{"layers":[{"mediaType":"application/vnd.devcontainers.layer.v1+tar","digest":"sha256:1111111111111111111111111111111111111111111111111111111111111111","size":12}]}"#;
        let actual_digest = sha256_digest(&hex_lower(&Sha256::digest(body)));

        let manifest = parse_registry_manifest_response_body(
            "ghcr.io/example/features/tool:1",
            None,
            None,
            body,
        )
        .unwrap();

        assert_eq!(manifest.digest, actual_digest);
        assert_eq!(manifest.layers.len(), 1);
    }

    #[test]
    fn docker_hub_registry_url_uses_registry_endpoint() {
        let reference = parse_oci_feature_ref("docker.io/example/features/tool:1").unwrap();

        assert_eq!(
            registry_url(&reference, "manifests/1"),
            "https://registry-1.docker.io/v2/example/features/tool/manifests/1"
        );
    }

    #[test]
    fn bracketed_ipv6_registry_url_preserves_host_port() {
        let reference =
            parse_oci_feature_ref("[2001:db8::1]:5000/example/features/tool:1").unwrap();

        assert_eq!(
            registry_url(&reference, "manifests/1"),
            "https://[2001:db8::1]:5000/v2/example/features/tool/manifests/1"
        );
    }

    #[test]
    fn bearer_challenge_parses_case_insensitive_scheme_and_parameters() {
        let challenge = parse_bearer_challenge(
            r#"bearer REALM="https://auth.example/token", SERVICE="registry.example", SCOPE="repository:example/features/tool:pull""#,
        )
        .unwrap();

        assert_eq!(challenge.realm, "https://auth.example/token");
        assert_eq!(challenge.service.as_deref(), Some("registry.example"));
        assert_eq!(
            challenge.scope.as_deref(),
            Some("repository:example/features/tool:pull")
        );
    }

    fn sha256_digest(hex: &str) -> String {
        format!("sha256:{hex}")
    }
}
