use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::fs;
use tokio::sync::mpsc::UnboundedSender;

use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};

use teos_common::appointment::{Appointment, Locator};
use teos_common::cryptography;
use teos_common::dbm::Error as DBError;
use teos_common::receipts::{AppointmentReceipt, RegistrationReceipt};
use teos_common::{UserId, UserId as TowerId};

use crate::dbm::DBM;
use crate::{MisbehaviorProof, TowerInfo, TowerStatus, TowerSummary};

/// Represents the watchtower client that is being used as the CoreLN plugin state.
#[derive(Clone)]
pub struct WTClient {
    /// A [DBM] instance.
    pub dbm: Arc<Mutex<DBM>>,
    /// A collection of towers the client is registered to.
    pub towers: HashMap<TowerId, TowerSummary>,
    /// Queue of unreachable towers
    pub unreachable_towers: UnboundedSender<TowerId>,
    /// The user secret key.
    pub user_sk: SecretKey,
    /// The user identifier.
    pub user_id: UserId,
}

impl WTClient {
    pub async fn new(data_dir: PathBuf, unreachable_towers: UnboundedSender<TowerId>) -> Self {
        // Create data dir if it does not exist
        fs::create_dir_all(&data_dir).await.unwrap_or_else(|e| {
            log::error!("Cannot create data dir: {:?}", e);
            std::process::exit(1);
        });

        let dbm = DBM::new(&data_dir.join("watchtowers_db.sql3")).unwrap();
        let (user_sk, user_id) = match dbm.load_client_key() {
            Ok(sk) => (
                sk,
                UserId(PublicKey::from_secret_key(&Secp256k1::new(), &sk)),
            ),
            Err(_) => {
                log::info!("Watchtower client keys not found. Creating a fresh set");
                let (sk, pk) = cryptography::get_random_keypair();
                dbm.store_client_key(&sk).unwrap();
                (sk, UserId(pk))
            }
        };

        let towers = dbm.load_towers();
        for (tower_id, tower) in towers.iter() {
            if tower.status.is_unreachable() {
                unreachable_towers.send(*tower_id).unwrap();
            }
        }

        log::info!(
            "Plugin watchtower client initialized. User id = {}",
            user_id
        );

        WTClient {
            towers,
            unreachable_towers,
            dbm: Arc::new(Mutex::new(dbm)),
            user_sk,
            user_id,
        }
    }

    /// Adds or updates a tower entry.
    pub fn add_update_tower(
        &mut self,
        tower_id: TowerId,
        tower_net_addr: String,
        receipt: &RegistrationReceipt,
    ) {
        self.dbm
            .lock()
            .unwrap()
            .store_tower_record(tower_id, &tower_net_addr, receipt)
            .unwrap();
        self.towers.insert(
            tower_id,
            TowerSummary::new(
                tower_net_addr,
                receipt.available_slots(),
                receipt.subscription_expiry(),
            ),
        );
    }

    /// Loads a tower record from the database.
    pub fn load_tower_info(&self, tower_id: TowerId) -> Result<TowerInfo, DBError> {
        self.dbm.lock().unwrap().load_tower_record(tower_id)
    }

    /// Sets the tower status to any of the `TowerStatus` variants.
    pub fn set_tower_status(&mut self, tower_id: TowerId, status: TowerStatus) {
        if let Some(tower) = self.towers.get_mut(&tower_id) {
            tower.status = status
        } else {
            log::error!(
                "Cannot change tower status to {}. Unknown tower_id: {}",
                status,
                tower_id
            );
        }
    }

    /// Adds an appointment receipt to the tower record.
    pub fn add_appointment_receipt(
        &mut self,
        tower_id: TowerId,
        locator: Locator,
        available_slots: u32,
        receipt: &AppointmentReceipt,
    ) {
        if let Some(tower) = self.towers.get_mut(&tower_id) {
            // DISCUSS: It may be nice to independently compute the slots and compare
            tower.available_slots = available_slots;

            self.dbm
                .lock()
                .unwrap()
                .store_appointment_receipt(tower_id, locator, available_slots, receipt)
                .unwrap();
        } else {
            log::error!(
                "Cannot add appointment receipt to tower. Unknown tower_id: {}",
                tower_id
            );
        }
    }

    /// Adds a pending appointment to the tower record.
    pub fn add_pending_appointment(&mut self, tower_id: TowerId, appointment: &Appointment) {
        if let Some(tower) = self.towers.get_mut(&tower_id) {
            tower.pending_appointments.insert(appointment.locator);

            self.dbm
                .lock()
                .unwrap()
                .store_pending_appointment(tower_id, appointment)
                .unwrap();
        } else {
            log::error!(
                "Cannot add pending appointment to tower. Unknown tower_id: {}",
                tower_id
            );
        }
    }

    /// Removes a pending appointment from the tower record.
    pub fn remove_pending_appointment(&mut self, tower_id: TowerId, locator: Locator) {
        if let Some(tower) = self.towers.get_mut(&tower_id) {
            tower.pending_appointments.remove(&locator);

            self.dbm
                .lock()
                .unwrap()
                .delete_pending_appointment(tower_id, locator)
                .unwrap();
        } else {
            log::error!(
                "Cannot remove pending appointment to tower. Unknown tower_id: {}",
                tower_id
            );
        }
    }

    /// Adds an invalid appointment to the tower record.
    pub fn add_invalid_appointment(&mut self, tower_id: TowerId, appointment: &Appointment) {
        if let Some(tower) = self.towers.get_mut(&tower_id) {
            tower.invalid_appointments.insert(appointment.locator);

            self.dbm
                .lock()
                .unwrap()
                .store_invalid_appointment(tower_id, appointment)
                .unwrap();
        } else {
            log::error!(
                "Cannot add invalid appointment to tower. Unknown tower_id: {}",
                tower_id
            );
        }
    }

    /// Flags a given tower as misbehaving, storing the misbehaving proof in the database.
    pub fn flag_misbehaving_tower(&mut self, tower_id: TowerId, proof: MisbehaviorProof) {
        if let Some(tower) = self.towers.get_mut(&tower_id) {
            self.dbm
                .lock()
                .unwrap()
                .store_misbehaving_proof(tower_id, &proof)
                .unwrap();
            tower.status = TowerStatus::Misbehaving;
        } else {
            log::error!("Cannot flag tower. Unknown tower_id: {}", tower_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::sync::mpsc::unbounded_channel;

    use teos_common::test_utils::{
        generate_random_appointment, get_random_appointment_receipt,
        get_random_registration_receipt, get_random_user_id,
    };

    #[tokio::test]
    async fn test_add_update_load_tower() {
        let tmp_path = &format!(".watchtower_{}/", get_random_user_id());
        let mut wt_client = WTClient::new(tmp_path.into(), unbounded_channel().0).await;

        // Adding a new tower will add a summary to towers and the full data to the
        let receipt = get_random_registration_receipt();
        let tower_id = get_random_user_id();
        let tower_info = TowerInfo::empty(
            "talaia.watch".into(),
            receipt.available_slots(),
            receipt.subscription_expiry(),
        );

        wt_client.add_update_tower(tower_id, tower_info.net_addr.clone(), &receipt);
        assert_eq!(
            wt_client.towers.get(&tower_id),
            Some(&TowerSummary::from(tower_info.clone()))
        );
        assert_eq!(wt_client.load_tower_info(tower_id).unwrap(), tower_info);

        // Calling the method again with updated information should also updated the records in memory and the database
        let receipt = get_random_registration_receipt();
        let updated_tower_info = TowerInfo::empty(
            "talaia.watch".into(),
            receipt.available_slots(),
            receipt.subscription_expiry(),
        );
        wt_client.add_update_tower(tower_id, updated_tower_info.net_addr.clone(), &receipt);

        assert_eq!(
            wt_client.towers.get(&tower_id),
            Some(&TowerSummary::from(updated_tower_info.clone()))
        );
        assert_eq!(
            wt_client.load_tower_info(tower_id).unwrap(),
            updated_tower_info
        );

        fs::remove_dir_all(tmp_path).await.unwrap();
    }

    #[tokio::test]
    async fn test_set_tower_status() {
        let tmp_path = &format!(".watchtower_{}/", get_random_user_id());
        let mut wt_client = WTClient::new(tmp_path.into(), unbounded_channel().0).await;

        // If the tower is unknown nothing will happen
        let unknown_tower = get_random_user_id();
        wt_client.set_tower_status(unknown_tower, TowerStatus::Reachable);
        assert!(!wt_client.towers.contains_key(&unknown_tower));

        // If the tower is known, the status will be updated.
        let receipt = get_random_registration_receipt();
        let tower_id = get_random_user_id();
        wt_client.add_update_tower(tower_id, "talaia.watch".into(), &receipt);

        for status in [
            TowerStatus::Reachable,
            TowerStatus::TemporaryUnreachable,
            TowerStatus::Unreachable,
            TowerStatus::SubscriptionError,
            TowerStatus::Misbehaving,
        ] {
            wt_client.set_tower_status(tower_id, status);
            assert_eq!(status, wt_client.towers.get(&tower_id).unwrap().status);
        }

        fs::remove_dir_all(tmp_path).await.unwrap();
    }

    #[tokio::test]
    async fn test_add_appointment_receipt() {
        let tmp_path = &format!(".watchtower_{}/", get_random_user_id());
        let mut wt_client = WTClient::new(tmp_path.into(), unbounded_channel().0).await;

        let (tower_sk, tower_pk) = cryptography::get_random_keypair();
        let tower_id = TowerId(tower_pk);
        let tower_net_addr = "talaia.watch";

        let locator = generate_random_appointment(None).locator;
        let registration_receipt = get_random_registration_receipt();
        let appointment_receipt = get_random_appointment_receipt(tower_sk);

        // If we call this on an unknown tower it will simply do nothing
        wt_client.add_appointment_receipt(
            tower_id,
            locator,
            registration_receipt.available_slots(),
            &appointment_receipt,
        );
        assert!(!wt_client.towers.contains_key(&tower_id));

        // Add the tower to the state and try again
        let tower_info = TowerInfo::new(
            tower_net_addr.into(),
            registration_receipt.available_slots(),
            registration_receipt.subscription_expiry(),
            HashMap::from([(locator, appointment_receipt.signature().unwrap())]),
            Vec::new(),
            Vec::new(),
        );
        wt_client.add_update_tower(tower_id, tower_net_addr.into(), &registration_receipt);
        wt_client.add_appointment_receipt(
            tower_id,
            locator,
            registration_receipt.available_slots(),
            &appointment_receipt,
        );

        assert!(wt_client.towers.contains_key(&tower_id));
        assert_eq!(
            wt_client.towers.get(&tower_id).unwrap(),
            &TowerSummary::from(tower_info.clone())
        );
        assert_eq!(wt_client.load_tower_info(tower_id).unwrap(), tower_info);

        fs::remove_dir_all(tmp_path).await.unwrap();
    }

    #[tokio::test]
    async fn test_add_pending_appointment() {
        let tmp_path = &format!(".watchtower_{}/", get_random_user_id());
        let mut wt_client = WTClient::new(tmp_path.into(), unbounded_channel().0).await;

        let tower_id = get_random_user_id();
        let tower_net_addr = "talaia.watch";

        let registration_receipt = get_random_registration_receipt();
        let appointment = generate_random_appointment(None);

        // If we call this on an unknown tower it will simply do nothing
        wt_client.add_pending_appointment(tower_id, &appointment);
        assert!(!wt_client.towers.contains_key(&tower_id));

        // Add the tower to the state and try again
        let tower_info = TowerInfo::new(
            tower_net_addr.into(),
            registration_receipt.available_slots(),
            registration_receipt.subscription_expiry(),
            HashMap::new(),
            vec![appointment.clone()],
            Vec::new(),
        );

        wt_client.add_update_tower(tower_id, tower_net_addr.into(), &registration_receipt);
        wt_client.add_pending_appointment(tower_id, &appointment);

        assert!(wt_client.towers.contains_key(&tower_id));
        assert_eq!(
            wt_client.towers.get(&tower_id).unwrap(),
            &TowerSummary::from(tower_info.clone())
        );
        // When towers data is loaded from the database, it is assumed to be reachable.
        assert_eq!(
            wt_client.load_tower_info(tower_id).unwrap(),
            tower_info.with_status(TowerStatus::TemporaryUnreachable)
        );

        fs::remove_dir_all(tmp_path).await.unwrap();
    }

    #[tokio::test]
    async fn test_remove_pending_appointment() {
        let tmp_path = &format!(".watchtower_{}/", get_random_user_id());
        let mut wt_client = WTClient::new(tmp_path.into(), unbounded_channel().0).await;

        let tower_id = get_random_user_id();
        let tower_net_addr = "talaia.watch";

        let registration_receipt = get_random_registration_receipt();
        let appointment = generate_random_appointment(None);

        // If we call this on an unknown tower it will simply do nothing
        wt_client.remove_pending_appointment(tower_id, appointment.locator);

        // Add the tower to the state and try again
        wt_client.add_update_tower(tower_id, tower_net_addr.into(), &registration_receipt);
        wt_client.add_pending_appointment(tower_id, &appointment);

        wt_client.remove_pending_appointment(tower_id, appointment.locator);
        assert!(!wt_client
            .towers
            .get(&tower_id)
            .unwrap()
            .pending_appointments
            .contains(&appointment.locator));
        // This bit is tested exhaustively in the DBM.
        assert!(!wt_client
            .dbm
            .lock()
            .unwrap()
            .appointment_exists(appointment.locator));

        fs::remove_dir_all(tmp_path).await.unwrap();
    }

    #[tokio::test]
    async fn test_add_invalid_appointment() {
        let tmp_path = &format!(".watchtower_{}/", get_random_user_id());
        let mut wt_client = WTClient::new(tmp_path.into(), unbounded_channel().0).await;

        let tower_id = get_random_user_id();
        let tower_net_addr = "talaia.watch";

        let registration_receipt = get_random_registration_receipt();
        let appointment = generate_random_appointment(None);

        // If we call this on an unknown tower it will simply do nothing
        wt_client.add_invalid_appointment(tower_id, &appointment);
        assert!(!wt_client.towers.contains_key(&tower_id));

        // Add the tower to the state and try again
        let tower_info = TowerInfo::new(
            tower_net_addr.into(),
            registration_receipt.available_slots(),
            registration_receipt.subscription_expiry(),
            HashMap::new(),
            Vec::new(),
            vec![appointment.clone()],
        );

        wt_client.add_update_tower(tower_id, tower_net_addr.into(), &registration_receipt);
        wt_client.add_invalid_appointment(tower_id, &appointment);

        assert!(wt_client.towers.contains_key(&tower_id));
        assert_eq!(
            wt_client.towers.get(&tower_id).unwrap(),
            &TowerSummary::from(tower_info.clone())
        );
        assert_eq!(wt_client.load_tower_info(tower_id).unwrap(), tower_info);

        fs::remove_dir_all(tmp_path).await.unwrap();
    }

    #[tokio::test]
    async fn test_move_pending_appointment_to_invalid() {
        let tmp_path = &format!(".watchtower_{}/", get_random_user_id());
        let mut wt_client = WTClient::new(tmp_path.into(), unbounded_channel().0).await;

        let tower_id = get_random_user_id();
        let tower_net_addr = "talaia.watch";

        let registration_receipt = get_random_registration_receipt();
        let appointment = generate_random_appointment(None);

        wt_client.add_update_tower(tower_id, tower_net_addr.into(), &registration_receipt);
        wt_client.add_pending_appointment(tower_id, &appointment);

        // Check that the appointment can be moved from pending to invalid
        wt_client.add_invalid_appointment(tower_id, &appointment);
        wt_client.remove_pending_appointment(tower_id, appointment.locator);

        assert!(!wt_client
            .towers
            .get(&tower_id)
            .unwrap()
            .pending_appointments
            .contains(&appointment.locator));
        assert!(wt_client
            .towers
            .get(&tower_id)
            .unwrap()
            .invalid_appointments
            .contains(&appointment.locator));
        assert!(!wt_client
            .dbm
            .lock()
            .unwrap()
            .load_appointment_locators(tower_id, crate::AppointmentStatus::Pending)
            .contains(&appointment.locator));
        assert!(wt_client
            .dbm
            .lock()
            .unwrap()
            .load_appointment_locators(tower_id, crate::AppointmentStatus::Invalid)
            .contains(&appointment.locator));
        assert!(wt_client
            .dbm
            .lock()
            .unwrap()
            .appointment_exists(appointment.locator));

        fs::remove_dir_all(tmp_path).await.unwrap();
    }

    #[tokio::test]
    async fn test_move_pending_appointment_to_invalid_multiple_towers() {
        // Check that moving an appointment from pending to invalid can be done even if multiple towers have a reference to it
        let tmp_path = &format!(".watchtower_{}/", get_random_user_id());
        let mut wt_client = WTClient::new(tmp_path.into(), unbounded_channel().0).await;

        let tower_id = get_random_user_id();
        let another_tower_id = get_random_user_id();
        let tower_net_addr = "talaia.watch";

        let registration_receipt = get_random_registration_receipt();
        let appointment = generate_random_appointment(None);

        wt_client.add_update_tower(tower_id, tower_net_addr.into(), &registration_receipt);
        wt_client.add_update_tower(
            another_tower_id,
            tower_net_addr.into(),
            &registration_receipt,
        );
        wt_client.add_pending_appointment(tower_id, &appointment);
        wt_client.add_pending_appointment(another_tower_id, &appointment);

        // Check that the appointment can be moved from pending to invalid
        wt_client.add_invalid_appointment(tower_id, &appointment);
        wt_client.remove_pending_appointment(tower_id, appointment.locator);

        // TOWER_ID CHECKS
        assert!(!wt_client
            .towers
            .get(&tower_id)
            .unwrap()
            .pending_appointments
            .contains(&appointment.locator));
        assert!(wt_client
            .towers
            .get(&tower_id)
            .unwrap()
            .invalid_appointments
            .contains(&appointment.locator));
        assert!(!wt_client
            .dbm
            .lock()
            .unwrap()
            .load_appointment_locators(tower_id, crate::AppointmentStatus::Pending)
            .contains(&appointment.locator));
        assert!(wt_client
            .dbm
            .lock()
            .unwrap()
            .load_appointment_locators(tower_id, crate::AppointmentStatus::Invalid)
            .contains(&appointment.locator));

        // ANOTHER_TOWER_ID CHECKS
        assert!(wt_client
            .towers
            .get(&another_tower_id)
            .unwrap()
            .pending_appointments
            .contains(&appointment.locator));
        assert!(!wt_client
            .towers
            .get(&another_tower_id)
            .unwrap()
            .invalid_appointments
            .contains(&appointment.locator));
        assert!(wt_client
            .dbm
            .lock()
            .unwrap()
            .load_appointment_locators(another_tower_id, crate::AppointmentStatus::Pending)
            .contains(&appointment.locator));
        assert!(!wt_client
            .dbm
            .lock()
            .unwrap()
            .load_appointment_locators(another_tower_id, crate::AppointmentStatus::Invalid)
            .contains(&appointment.locator));

        // GENERAL
        assert!(wt_client
            .dbm
            .lock()
            .unwrap()
            .appointment_exists(appointment.locator));

        fs::remove_dir_all(tmp_path).await.unwrap();
    }

    #[tokio::test]
    async fn test_flag_misbehaving_tower() {
        let tmp_path = &format!(".watchtower_{}/", get_random_user_id());
        let mut wt_client = WTClient::new(tmp_path.into(), unbounded_channel().0).await;

        let (tower_sk, tower_pk) = cryptography::get_random_keypair();
        let tower_id = TowerId(tower_pk);
        let tower_net_addr = "talaia.watch";

        // If we call this on an unknown tower it will simply do nothing
        let appointment = generate_random_appointment(None);
        let receipt = get_random_appointment_receipt(tower_sk);
        let proof = MisbehaviorProof::new(appointment.locator, receipt, get_random_user_id());
        wt_client.flag_misbehaving_tower(tower_id, proof.clone());
        assert!(!wt_client.towers.contains_key(&tower_id));

        // // Add the tower to the state and try again
        let registration_receipt = get_random_registration_receipt();
        wt_client.add_update_tower(tower_id, tower_net_addr.into(), &registration_receipt);
        wt_client.flag_misbehaving_tower(tower_id, proof.clone());

        // Check data in memory
        let tower_summary = wt_client.towers.get(&tower_id);
        assert!(tower_summary.is_some());
        assert_eq!(tower_summary.unwrap().status, TowerStatus::Misbehaving);

        // Check data in DB
        let loaded_info = wt_client.load_tower_info(tower_id).unwrap();
        assert_eq!(loaded_info.status, TowerStatus::Misbehaving);
        assert_eq!(loaded_info.misbehaving_proof, Some(proof));
        assert!(loaded_info.appointments.contains_key(&appointment.locator));

        fs::remove_dir_all(tmp_path).await.unwrap();
    }
}
