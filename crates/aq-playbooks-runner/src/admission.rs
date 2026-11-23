//! Immutable namespace-scoped manual admission receipts. The run and key receipt
//! share one storage CAS. Recovery lookup never grants new execution authority.
use crate::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyedAdmissionRequest {
    pub key: String,
    pub binding: String,
    pub expected_activation_revision: u64,
    pub entry: String,
    pub input: Value,
    /// Authenticated and derived by the host, never workflow input authority.
    pub authority: Authority,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeyedAdmissionReceipt {
    pub key: String,
    pub request_digest: String,
    pub run_id: String,
    pub activation_revision: u64,
    pub catalog_revision: u64,
    pub accepted_at_ms: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyedAdmissionDisposition {
    pub receipt: KeyedAdmissionReceipt,
    pub duplicate: bool,
}
fn valid_text(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.contains('\0')
}
fn valid_authority(authority: &Authority) -> bool {
    valid_text(&authority.actor, 512)
        && valid_text(&authority.scope, 512)
        && authority.capabilities.len() <= 64
        && authority
            .capabilities
            .iter()
            .all(|capability| valid_text(capability, 128))
}
impl KeyedAdmissionRequest {
    fn validate(&self) -> Result<()> {
        let address = valid_text(&self.key, 256)
            && valid_text(&self.binding, 512)
            && valid_text(&self.entry, 512)
            && self.expected_activation_revision != 0;
        if !address || !valid_authority(&self.authority) {
            return Err(Error::Refused(
                "invalid keyed admission identity or authority bounds".into(),
            ));
        }
        struct Count(usize);
        impl std::io::Write for Count {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if bytes.len() > self.0 {
                    return Err(std::io::Error::other("keyed admission byte bound"));
                }
                self.0 -= bytes.len();
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        serde_json::to_writer(Count(1_048_576), self)
            .map_err(|_| Error::Refused("keyed admission byte bound".into()))?;
        Ok(())
    }
}
impl<S: Storage> Engine<S> {
    /// Historical lookup only. Hosts must authenticate visibility separately.
    /// A receipt stays until its window ends (later while its run is live);
    /// its run need not still be in the State.
    pub fn admission_receipt(&self, key: &str) -> Result<Option<KeyedAdmissionReceipt>> {
        if !valid_text(key, 256) {
            return Err(Error::Refused("invalid keyed admission identity".into()));
        }
        Ok(self
            .store
            .load(&self.namespace)?
            .state
            .keyed_admissions
            .get(key)
            .cloned())
    }
    /// Exact replay performs no write, new resolution, or authority grant.
    /// First admission creates run+receipt atomically. A key is remembered
    /// until its window (`admission_replay_ms`) expires.
    pub fn admit_once(
        &self,
        request: KeyedAdmissionRequest,
        now_ms: i64,
    ) -> Result<KeyedAdmissionDisposition> {
        self.admit_once_at(request, None, now_ms)
    }
    /// [`Self::admit_once`] with the producer's request time (not part of
    /// the request digest, never stored). A key the engine still remembers
    /// answers as before, whatever its request time. Otherwise a request
    /// time older than `max(now, clock) - policy.admission_replay_ms` is
    /// refused with [`Error::Stale`] and nothing is written. `None`
    /// is not checked: an expired key is then accepted as new.
    pub fn admit_once_at(
        &self,
        request: KeyedAdmissionRequest,
        requested_at_ms: Option<i64>,
        now_ms: i64,
    ) -> Result<KeyedAdmissionDisposition> {
        request.validate()?;
        let admission = Admission {
            request: &request,
            request_digest: digest(&request)?,
            requested_at_ms,
            now_ms,
        };
        // An expired receipt leaves in its own commit before a new admission.
        for _ in 0..2 {
            let disposition = self.transaction_choice_at(Some(now_ms), |s| admission.commit(s))?;
            if let Some(disposition) = disposition {
                return Ok(disposition);
            }
        }
        Err(Error::Conflict(
            "keyed admission receipt changed during admission".into(),
        ))
    }
}

/// Storage adapters reject rebinding a receipt, and removing one before its
/// window expires, even through their public CAS API. This is part of the
/// shared removal check, [`crate::retention::verify_removals`].
pub fn verify_receipt_retention(
    previous: &State,
    next: &State,
) -> std::result::Result<(), StorageError> {
    crate::retention::verify_removals(previous, next).map(|_| ())
}
struct Admission<'a> {
    request: &'a KeyedAdmissionRequest,
    request_digest: String,
    requested_at_ms: Option<i64>,
    now_ms: i64,
}
type Commit = Result<(Option<KeyedAdmissionDisposition>, bool)>;
impl Admission<'_> {
    /// No disposition means this commit forgot the expired receipt.
    fn commit(&self, s: &mut State) -> Commit {
        if let Some(receipt) = s.keyed_admissions.get(&self.request.key) {
            // Reuse the immutable receipt without a write.
            let start = retention::window_start(s, self.now_ms);
            if !retention::admission_droppable(s, &self.request.key, start) {
                return self.duplicate(receipt);
            }
            self.check_time(s)?;
            retention::forget_admission(s, &self.request.key);
            return Ok((None, true));
        }
        self.check_time(s)?;
        let activation = self.request.activation(s)?;
        let dependencies = self.request.dependencies(s, &activation)?;
        let run_id = next_id(s, "run");
        admit_run(
            s,
            run_id.clone(),
            run_id.clone(),
            None,
            None,
            &activation.artifact,
            &self.request.entry,
            self.request.input.clone(),
            self.request.authority.clone(),
            dependencies,
            0,
            self.now_ms,
        )?;
        let receipt = KeyedAdmissionReceipt {
            key: self.request.key.clone(),
            request_digest: self.request_digest.clone(),
            run_id,
            activation_revision: activation.revision,
            catalog_revision: s.catalog_revision,
            accepted_at_ms: self.now_ms,
        };
        receipt.remember(s);
        Ok((
            Some(KeyedAdmissionDisposition {
                receipt,
                duplicate: false,
            }),
            true,
        ))
    }
    fn check_time(&self, s: &State) -> Result<()> {
        refuse_stale(
            s,
            self.requested_at_ms,
            || s.retention.policy.admission_replay_ms,
            self.now_ms,
        )
    }
    fn duplicate(&self, receipt: &KeyedAdmissionReceipt) -> Commit {
        if receipt.request_digest != self.request_digest {
            return Err(Error::Conflict(
                "keyed admission identity reused with changed content".into(),
            ));
        }
        Ok((
            Some(KeyedAdmissionDisposition {
                receipt: receipt.clone(),
                duplicate: true,
            }),
            false,
        ))
    }
}

impl KeyedAdmissionRequest {
    fn activation(&self, s: &State) -> Result<Activation> {
        let activation = s
            .bindings
            .get(&self.binding)
            .ok_or_else(|| Error::NotFound(self.binding.clone()))?
            .clone();
        if activation.revision != self.expected_activation_revision {
            return Err(Error::Conflict(
                "keyed admission activation revision".into(),
            ));
        }
        if !activation.controls.calls_enabled || !activation.controls.execution_enabled {
            return Err(Error::Refused("keyed admission activation disabled".into()));
        }
        let principal_matches = activation.authority.actor == self.authority.actor
            && activation.authority.scope == self.authority.scope;
        if !principal_matches
            || !self
                .authority
                .capabilities
                .is_subset(&activation.authority.capabilities)
        {
            return Err(Error::Refused(
                "keyed admission activation authority ceiling".into(),
            ));
        }
        Ok(activation)
    }
    fn dependencies(&self, s: &State, activation: &Activation) -> Result<BTreeMap<String, String>> {
        let dependencies = resolve(s, &activation.artifact, &self.authority)?;
        for artifact in std::iter::once(&activation.artifact).chain(dependencies.values()) {
            let selected = s
                .artifacts
                .get(artifact)
                .ok_or_else(|| Error::NotFound(artifact.clone()))?;
            if !execution_allowed(
                s,
                artifact,
                &self.authority,
                &selected.content.playbook.required_capabilities,
            )? {
                return Err(Error::Refused(
                    "keyed admission execution authority revoked".into(),
                ));
            }
        }
        Ok(dependencies)
    }
}

impl KeyedAdmissionReceipt {
    fn remember(&self, s: &mut State) {
        s.keyed_admissions.insert(self.key.clone(), self.clone());
        // Held by its live run: it joins the eviction order when the
        // run leaves (`retention::rejoin`).
        s.retention
            .replay_held
            .insert(self.run_id.clone(), self.key.clone());
        let expires_at_ms = retention::window_start(s, self.accepted_at_ms).saturating_add(
            i64::try_from(s.retention.policy.admission_replay_ms).unwrap_or(i64::MAX),
        );
        s.retention
            .admission_expires_at_ms
            .insert(self.key.clone(), expires_at_ms);
        retention::window_insert(
            s,
            expires_at_ms,
            format!("{}{}", retention::WINDOW_ADMISSION, self.key),
        );
    }
}
