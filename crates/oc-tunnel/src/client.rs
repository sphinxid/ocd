//! `OpenTunnelClient` (`client/src/effect/client.ts`): creates the profile's tunnel, issues and renews its
//! certificate, and connects routes.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use url::Url;

use crate::api::{Api, ApiError, CertificateState, DEFAULT_API};
use crate::csr::TunnelKey;
use crate::proxy::{Env, Transport};
use crate::storage::{Identity, Pending, Storage, parse_date};
use crate::tunnel::{self, Event, Fatal, OnRouteConflict, Routes, Tunnel};
use crate::{Cause, Error};

/// Provisioning progress (`onProgress`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// `ensure` found a stored tunnel and is checking its certificate with the server.
    CheckingCertificate,
    CreatingTunnel,
    GeneratingKey,
    GeneratingCsr,
    RequestingCertificate,
    WaitingCertificate,
    ResumingCertificate,
    SavingIdentity,
    Ready,
}

type OnProgress<'a> = Option<&'a (dyn Fn(Progress) + Send + Sync)>;

/// What a failure's cause adds to its message (`describeApiError`).
fn detail(cause: &Cause) -> String {
    match cause {
        Cause::Api(e) => e.describe(),
        other => other.to_string(),
    }
}

/// `clientError`: the message with what went wrong appended.
fn client_error(message: &str, cause: Cause) -> Error {
    Error::Client { message: format!("{message}: {}", detail(&cause)), cause: Some(cause) }
}

fn api_error(message: &str, error: ApiError) -> Error {
    client_error(message, Cause::Api(error))
}

/// `tunnelError`: a failure of the running tunnel, carrying the bridge's attach error code when there is one.
fn tunnel_error(message: &str, fatal: Fatal) -> Error {
    client_error(
        message,
        match fatal {
            Fatal::Attach(e) => Cause::Attach(e.code),
            Fatal::Error(m) => Cause::Message(m),
        },
    )
}

/// An invalid route, as `clientError(cause.message, cause)` reports it (the message twice).
fn routes_error(message: String) -> Error {
    client_error(&message.clone(), Cause::Message(message))
}

fn progress(on: OnProgress<'_>, stage: Progress) {
    if let Some(f) = on {
        f(stage)
    }
}

/// `OpenTunnelClientOptions` beyond the API and the store.
#[derive(Clone, Default)]
pub struct ClientOptions {
    /// An `http://` or `https://` proxy URL (credentials as userinfo) for the API and the bridge, instead of the one
    /// the environment selects.
    pub proxy: Option<String>,
    /// Where the proxy variables are read (`process.env`); the process environment when `None`.
    pub env: Option<Env>,
}

/// `OpenTunnelConnectOptions` beyond the profile.
#[derive(Clone, Default)]
pub struct ConnectOptions {
    pub routes: Routes,
    pub on_route_conflict: OnRouteConflict,
    /// Called with every event as it happens, including those before `connect` completes.
    pub on_event: Option<Arc<dyn Fn(Event) + Send + Sync>>,
}

#[derive(Clone)]
pub struct Client {
    api: Api,
    storage: Storage,
}

/// A connected tunnel (`Connection`).
pub struct Connection {
    pub tunnel: Identity,
    running: Tunnel,
}

impl Connection {
    /// Resolves when the tunnel stops: `Ok` after [`Connection::close`], `Tunnel stopped: <why>` otherwise.
    pub async fn closed(&self) -> Result<(), Error> {
        self.running.closed().await.map_err(|e| tunnel_error("Tunnel stopped", e))
    }

    pub async fn close(&self) {
        self.running.close().await
    }

    pub fn status(&self) -> tunnel::Status {
        self.running.status()
    }

    /// The public URL of each current route; follows `set_routes`.
    pub fn urls(&self) -> BTreeMap<String, String> {
        self.running.urls()
    }

    pub fn set_routes(&self, routes: Routes) -> Result<(), Error> {
        self.running.set_routes(routes).map_err(routes_error)
    }
}

impl Client {
    /// `OpenTunnelClient.layer({ api, storage })`, through the proxies the process environment selects.
    pub fn new(api: Url, storage: Storage) -> Client {
        Client { api: Api::new(api), storage }
    }

    /// `OpenTunnelClient.layer({ api, storage, proxy })`. Fails like `layer` when the proxy cannot be used.
    pub fn with_options(api: Url, storage: Storage, options: ClientOptions) -> Result<Client, Error> {
        let env = options.env.unwrap_or_else(crate::proxy::process_env);
        let transport = Transport::new(options.proxy.as_deref(), env)
            .map_err(|m| Error::Client { message: m.clone(), cause: Some(Cause::Message(m)) })?;
        Ok(Client { api: Api::with_transport(api, transport), storage })
    }

    /// `OpenTunnelClient.layer()`: the public service and the XDG store.
    pub fn default_client() -> Client {
        Client::new(Url::parse(DEFAULT_API).expect("api url"), Storage::xdg())
    }

    /// Test hook: scales the API's retry delays.
    pub fn set_retry_scale(&mut self, scale: f64) {
        self.api.set_retry_scale(scale);
    }

    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    pub fn api(&self) -> &Api {
        &self.api
    }

    pub fn get(&self, profile: &str) -> Result<Option<Identity>, Error> {
        self.storage.load(profile)
    }

    pub fn pending(&self, profile: &str) -> Result<Option<(String, String)>, Error> {
        Ok(self.storage.load_pending(profile)?.map(|p| (p.id, p.hostname)))
    }

    async fn complete_pending(&self, profile: &str, pending: Pending, on: OnProgress<'_>) -> Result<Identity, Error> {
        progress(on, Progress::RequestingCertificate);
        match self.api.bind_certificate(&pending.token, &pending.id, &pending.csr).await {
            Ok(_) => {}
            // A resumed provision may already have an issuance in flight.
            Err(e) if e.tag.as_deref() == Some("CertificateInProgressError") => {}
            Err(e) => return Err(api_error("Failed to start certificate issuance", e)),
        }
        let (certificate, chain, expiry) = loop {
            progress(on, Progress::WaitingCertificate);
            let value = self
                .api
                .get_certificate(&pending.token, &pending.id)
                .await
                .map_err(|e| api_error("Failed to read certificate", e))?;
            match value.state {
                CertificateState::Ready { certificate, chain, expiry } => break (certificate, chain, expiry),
                CertificateState::Failed { reason } => {
                    return Err(Error::Client {
                        message: format!("Certificate issuance failed: {reason}"),
                        cause: None,
                    });
                }
                _ => tokio::time::sleep(Duration::from_secs(2)).await,
            }
        };
        let identity = Identity {
            id: pending.id,
            hostname: pending.hostname,
            token: pending.token,
            private_key: pending.private_key,
            certificate,
            chain,
            certificate_expiry: parse_date(&expiry)
                .ok_or_else(|| Error::Defect(format!("Invalid certificate expiry: {expiry}")))?,
        };
        progress(on, Progress::SavingIdentity);
        self.storage.save(profile, &identity)?;
        progress(on, Progress::Ready);
        Ok(identity)
    }

    async fn provision(
        &self,
        profile: &str,
        id: String,
        hostname: String,
        token: String,
        on: OnProgress<'_>,
    ) -> Result<Identity, Error> {
        progress(on, Progress::GeneratingKey);
        let key =
            TunnelKey::generate().map_err(|e| client_error("Failed to generate certificate key", Cause::Message(e)))?;
        progress(on, Progress::GeneratingCsr);
        let csr = key
            .certificate_request(&hostname)
            .map_err(|e| client_error("Failed to generate certificate request", Cause::Message(e)))?;
        let pending = Pending { id, hostname, token, private_key: key.pem.clone(), csr };
        self.storage.save_pending(profile, &pending)?;
        self.complete_pending(profile, pending, on).await
    }

    /// `tunnel.create`
    pub async fn create(&self, profile: &str, on: OnProgress<'_>) -> Result<Identity, Error> {
        if self.storage.load(profile)?.is_some() {
            return Err(Error::Client { message: format!("Profile '{profile}' already has a tunnel"), cause: None });
        }
        if let Some(pending) = self.storage.load_pending(profile)? {
            progress(on, Progress::ResumingCertificate);
            return self.complete_pending(profile, pending, on).await;
        }
        progress(on, Progress::CreatingTunnel);
        let created = self.api.create().await.map_err(|e| api_error("Failed to create tunnel", e))?;
        self.provision(profile, created.tunnel.id, created.tunnel.hostname, created.token, on).await
    }

    /// `tunnel.ensure`: the profile's tunnel, created when missing, with a certificate the server renewed while
    /// this device was offline, re-issued when issuance failed.
    pub async fn ensure(&self, profile: &str) -> Result<Identity, Error> {
        self.ensure_with_progress(profile, None).await
    }

    /// `tunnel.ensure({ onProgress })`
    pub async fn ensure_with_progress(&self, profile: &str, on: OnProgress<'_>) -> Result<Identity, Error> {
        let Some(existing) = self.storage.load(profile)? else {
            return self.create(profile, on).await;
        };
        progress(on, Progress::CheckingCertificate);
        let certificate = match self.api.get_certificate(&existing.token, &existing.id).await {
            Ok(certificate) => certificate,
            // The API is unreachable even after retries: a certificate that has not expired still serves, and a
            // running tunnel checks for a renewed one itself.
            Err(e) if e.is_transient() && existing.certificate_expiry > chrono::Utc::now() => {
                progress(on, Progress::Ready);
                return Ok(existing);
            }
            Err(e) => return Err(api_error("Failed to read certificate", e)),
        };
        match certificate.state {
            CertificateState::Ready { certificate, chain, expiry } if certificate != existing.certificate => {
                let renewed = Identity {
                    certificate,
                    chain,
                    certificate_expiry: parse_date(&expiry)
                        .ok_or_else(|| Error::Defect(format!("Invalid certificate expiry: {expiry}")))?,
                    ..existing
                };
                progress(on, Progress::SavingIdentity);
                self.storage.save(profile, &renewed)?;
                progress(on, Progress::Ready);
                Ok(renewed)
            }
            CertificateState::Failed { .. } => {
                self.provision(profile, existing.id, existing.hostname, existing.token, on).await
            }
            _ => {
                progress(on, Progress::Ready);
                Ok(existing)
            }
        }
    }

    /// `tunnel.resume`
    pub async fn resume(&self, profile: &str, on: OnProgress<'_>) -> Result<Option<Identity>, Error> {
        let Some(pending) = self.storage.load_pending(profile)? else {
            return Ok(None);
        };
        progress(on, Progress::ResumingCertificate);
        self.complete_pending(profile, pending, on).await.map(Some)
    }

    /// `tunnel.remove`
    pub async fn remove(&self, profile: &str) -> Result<(), Error> {
        let Some(identity) = self.storage.load(profile)? else {
            return Ok(());
        };
        self.api.remove(&identity.token, &identity.id).await.map_err(|e| api_error("Failed to remove tunnel", e))?;
        self.storage.remove(profile)
    }

    /// `tunnel.connect({ routes, onEvent })`
    pub async fn connect(
        &self,
        profile: &str,
        routes: Routes,
        on_event: Option<Arc<dyn Fn(Event) + Send + Sync>>,
    ) -> Result<Connection, Error> {
        self.connect_with(profile, ConnectOptions { routes, on_event, ..Default::default() }).await
    }

    /// `tunnel.connect`: ensures the tunnel, starts it and waits for the bridge to attach. Dropping the future while
    /// it attaches stops the tunnel.
    pub async fn connect_with(&self, profile: &str, options: ConnectOptions) -> Result<Connection, Error> {
        crate::routes::validate_routes(&options.routes).map_err(routes_error)?;
        let identity = self.ensure(profile).await?;
        let storage = self.storage.clone();
        let profile_name = profile.to_string();
        let mut running = Tunnel::start(tunnel::Options {
            api: self.api.clone(),
            identity: identity.clone(),
            routes: options.routes,
            on_route_conflict: options.on_route_conflict,
            on_event: options.on_event.unwrap_or_else(|| Arc::new(|_| {})),
            on_renewed: Arc::new(move |renewed| {
                let _ = storage.save(&profile_name, &renewed);
            }),
        })
        .map_err(Error::Defect)?;
        if let Err(e) = running.ready().await {
            running.close().await;
            return Err(tunnel_error("Failed to connect tunnel", e));
        }
        Ok(Connection { tunnel: identity, running })
    }
}
