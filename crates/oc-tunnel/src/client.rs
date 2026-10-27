//! `OpenTunnelClient` (`client/src/effect/client.ts`): creates the profile's tunnel, issues and renews its
//! certificate, and connects routes.

use std::sync::Arc;
use std::time::Duration;

use url::Url;

use crate::api::{Api, CertificateState, DEFAULT_API};
use crate::csr::TunnelKey;
use crate::storage::{Identity, Pending, Storage, parse_date};
use crate::tunnel::{self, Event, Routes, Tunnel};
use crate::{Cause, Error};

/// Provisioning progress (`onProgress`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
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

fn client_error(message: &str, cause: Cause) -> Error {
    Error::Client { message: message.to_string(), cause: Some(cause) }
}

fn progress(on: OnProgress<'_>, stage: Progress) {
    if let Some(f) = on {
        f(stage)
    }
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
    /// Resolves when the tunnel stops: `Ok` after [`Connection::close`], `Tunnel stopped` otherwise.
    pub async fn closed(&self) -> Result<(), Error> {
        self.running.closed().await.map_err(|e| client_error("Tunnel stopped", Cause::Attach(e.code)))
    }

    pub async fn close(&self) {
        self.running.close().await
    }

    pub fn status(&self) -> tunnel::Status {
        self.running.status()
    }

    pub fn set_routes(&self, routes: Routes) -> Result<(), Error> {
        self.running.set_routes(routes).map_err(|m| Error::Client { message: m, cause: None })
    }
}

impl Client {
    /// `OpenTunnelClient.layer({ api, storage })`.
    pub fn new(api: Url, storage: Storage) -> Client {
        Client { api: Api::new(api), storage }
    }

    /// `OpenTunnelClient.layer()`: the public service and the XDG store.
    pub fn default_client() -> Client {
        Client::new(Url::parse(DEFAULT_API).expect("api url"), Storage::xdg())
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
            Err(e) => {
                return Err(client_error("Failed to start certificate issuance", Cause::Api(e)));
            }
        }
        let (certificate, chain, expiry) = loop {
            progress(on, Progress::WaitingCertificate);
            let value = self
                .api
                .get_certificate(&pending.token, &pending.id)
                .await
                .map_err(|e| client_error("Failed to read certificate", Cause::Api(e)))?;
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
        let created = self.api.create().await.map_err(|e| client_error("Failed to create tunnel", Cause::Api(e)))?;
        self.provision(profile, created.tunnel.id, created.tunnel.hostname, created.token, on).await
    }

    /// `tunnel.ensure`: the profile's tunnel, created when missing, with a certificate the server renewed while
    /// this device was offline, re-issued when issuance failed.
    pub async fn ensure(&self, profile: &str) -> Result<Identity, Error> {
        let Some(existing) = self.storage.load(profile)? else {
            return self.create(profile, None).await;
        };
        let certificate = self
            .api
            .get_certificate(&existing.token, &existing.id)
            .await
            .map_err(|e| client_error("Failed to read certificate", Cause::Api(e)))?;
        match certificate.state {
            CertificateState::Ready { certificate, chain, expiry } if certificate != existing.certificate => {
                let renewed = Identity {
                    certificate,
                    chain,
                    certificate_expiry: parse_date(&expiry)
                        .ok_or_else(|| Error::Defect(format!("Invalid certificate expiry: {expiry}")))?,
                    ..existing
                };
                self.storage.save(profile, &renewed)?;
                Ok(renewed)
            }
            CertificateState::Failed { .. } => {
                self.provision(profile, existing.id, existing.hostname, existing.token, None).await
            }
            _ => Ok(existing),
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
        self.api
            .remove(&identity.token, &identity.id)
            .await
            .map_err(|e| client_error("Failed to remove tunnel", Cause::Api(e)))?;
        self.storage.remove(profile)
    }

    /// `tunnel.connect`: ensures the tunnel, starts it and waits for the bridge to attach.
    pub async fn connect(
        &self,
        profile: &str,
        routes: Routes,
        on_event: Option<Arc<dyn Fn(Event) + Send + Sync>>,
    ) -> Result<Connection, Error> {
        crate::names::validate_routes(routes.iter()).map_err(|m| Error::Client { message: m, cause: None })?;
        let identity = self.ensure(profile).await?;
        let storage = self.storage.clone();
        let profile_name = profile.to_string();
        let mut running = Tunnel::start(tunnel::Options {
            api: self.api.clone(),
            identity: identity.clone(),
            routes,
            on_event: on_event.unwrap_or_else(|| Arc::new(|_| {})),
            on_renewed: Arc::new(move |renewed| {
                let _ = storage.save(&profile_name, &renewed);
            }),
        })
        .map_err(Error::Defect)?;
        if let Err(e) = running.ready().await {
            running.close().await;
            return Err(client_error("Failed to connect tunnel", Cause::Attach(e.code)));
        }
        Ok(Connection { tunnel: identity, running })
    }
}
