#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PublicV1ControllerRegistration {
    Reconciler,
    PodOwner,
    PersistentVolumeClaimOwner,
    ServiceOwner,
    SecretOwner,
}

impl PublicV1ControllerRegistration {
    #[allow(dead_code)]
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Reconciler => "reconciler",
            Self::PodOwner => "owner:Pod",
            Self::PersistentVolumeClaimOwner => "owner:PersistentVolumeClaim",
            Self::ServiceOwner => "owner:Service",
            Self::SecretOwner => "owner:Secret",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ReconcilerStartup;

#[derive(Debug, Clone, Copy)]
struct PodOwnerStartup;

#[derive(Debug, Clone, Copy)]
struct PersistentVolumeClaimOwnerStartup;

#[derive(Debug, Clone, Copy)]
struct ServiceOwnerStartup;

#[derive(Debug, Clone, Copy)]
struct SecretOwnerStartup;

#[derive(Debug, Clone, Copy)]
pub(crate) struct PublicV1ControllerManifest {
    reconciler: ReconcilerStartup,
    pod_owner: PodOwnerStartup,
    persistent_volume_claim_owner: PersistentVolumeClaimOwnerStartup,
    service_owner: ServiceOwnerStartup,
    secret_owner: SecretOwnerStartup,
}

pub(crate) struct PublicV1ControllerOwners<P, V, S, C> {
    pub(crate) pods: P,
    pub(crate) persistent_volume_claims: V,
    pub(crate) services: S,
    pub(crate) secrets: C,
}

impl PublicV1ControllerManifest {
    pub(crate) const fn registrations(self) -> [PublicV1ControllerRegistration; 5] {
        [
            PublicV1ControllerRegistration::Reconciler,
            PublicV1ControllerRegistration::PodOwner,
            PublicV1ControllerRegistration::PersistentVolumeClaimOwner,
            PublicV1ControllerRegistration::ServiceOwner,
            PublicV1ControllerRegistration::SecretOwner,
        ]
    }

    pub(crate) fn bind_reconciler<T>(self, reconciler: T) -> T {
        let Self {
            reconciler: owner, ..
        } = self;
        let _ = owner;
        reconciler
    }

    pub(crate) fn bind_owners<P, V, S, C>(
        self,
        pods: P,
        persistent_volume_claims: V,
        services: S,
        secrets: C,
    ) -> PublicV1ControllerOwners<P, V, S, C> {
        let Self {
            pod_owner,
            persistent_volume_claim_owner,
            service_owner,
            secret_owner,
            ..
        } = self;
        let _ = (
            pod_owner,
            persistent_volume_claim_owner,
            service_owner,
            secret_owner,
        );
        PublicV1ControllerOwners {
            pods,
            persistent_volume_claims,
            services,
            secrets,
        }
    }
}

pub(crate) const PUBLIC_V1_CONTROLLER: PublicV1ControllerManifest = PublicV1ControllerManifest {
    reconciler: ReconcilerStartup,
    pod_owner: PodOwnerStartup,
    persistent_volume_claim_owner: PersistentVolumeClaimOwnerStartup,
    service_owner: ServiceOwnerStartup,
    secret_owner: SecretOwnerStartup,
};

pub(crate) fn validate_public_v1_controller_registrations(
    manifest: PublicV1ControllerManifest,
) -> Result<(), &'static str> {
    if manifest.registrations()
        != [
            PublicV1ControllerRegistration::Reconciler,
            PublicV1ControllerRegistration::PodOwner,
            PublicV1ControllerRegistration::PersistentVolumeClaimOwner,
            PublicV1ControllerRegistration::ServiceOwner,
            PublicV1ControllerRegistration::SecretOwner,
        ]
    {
        return Err("Public V1 controller registrations changed");
    }
    Ok(())
}
