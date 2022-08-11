use std::convert::TryFrom;
use std::env;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use home::home_dir;
use serde_json::json;
use tokio::io::{stdin, stdout};
use tokio::sync::mpsc::unbounded_channel;

use cln_plugin::options::{ConfigOption, Value};
use cln_plugin::{anyhow, Builder, Error, Plugin};

use teos_common::appointment::{Appointment, Locator};
use teos_common::protos as common_msgs;
use teos_common::receipts::RegistrationReceipt;
use teos_common::TowerId;
use teos_common::{cryptography, errors};

use watchtower_plugin::convert::{CommitmentRevocation, GetAppointmentParams, RegisterParams};
use watchtower_plugin::net::http::{
    add_appointment, post_request, process_post_response, AddAppointmentError, ApiResponse,
    RequestError,
};
use watchtower_plugin::retrier::Retrier;
use watchtower_plugin::wt_client::WTClient;
use watchtower_plugin::TowerStatus;

fn to_cln_error(e: RequestError) -> Error {
    match e {
        RequestError::ConnectionError(e) => anyhow!(e),
        RequestError::DeserializeError(e) => anyhow!(e),
        RequestError::Unexpected(e) => anyhow!(e),
    }
}

/// Registers the client to a given tower.
///
/// Accepted tower_id formats:
///     - tower_id@host:port
///     - tower_id host port
///     - tower_id@host (will default port to DEFAULT_PORT)
///     - tower_id host (will default port to DEFAULT_PORT)
async fn register(
    plugin: Plugin<Arc<Mutex<WTClient>>>,
    v: serde_json::Value,
) -> Result<serde_json::Value, Error> {
    let params = RegisterParams::try_from(v).map_err(|x| anyhow!(x))?;
    let host = params.host.unwrap_or_else(|| "localhost".into());
    let tower_id = params.tower_id;
    let user_id = plugin.state().lock().unwrap().user_id;

    // TODO: The user should pick the start_time or, at least, check the returned start time against it's known block height.
    // Otherwise the tower could just generate a subscription starting far in the future. For this we need to access lightning RPC
    // which is not available in the current version of `cln-plugin` (but already on master). Add it for the next release.

    // FIXME: This is a workaround. Ideally, `cln_plugin::options::Value` will implement `as_u64` so we can simply call and unwrap
    // given that we are certain the option exists.
    let port = params.port.unwrap_or(
        if let Value::Integer(x) = plugin.option("watchtower-port").unwrap() {
            x as u16
        } else {
            // We will never end up here, but we need to define an else. Should be fixed alongside the previous fixme.
            9814
        },
    );

    let mut tower_net_addr = format!("{}:{}", host, port);
    if !tower_net_addr.starts_with("http") {
        tower_net_addr = format!("http://{}", tower_net_addr)
    }

    let register_endpoint = format!("{}/register", tower_net_addr);
    log::info!("Registering in the Eye of Satoshi (tower_id={})", tower_id);

    let receipt = process_post_response(
        post_request(
            &register_endpoint,
            &common_msgs::RegisterRequest {
                user_id: user_id.to_vec(),
            },
        )
        .await,
    )
    .await
    .map(|r: common_msgs::RegisterResponse| {
        RegistrationReceipt::with_signature(
            user_id,
            r.available_slots,
            r.subscription_start,
            r.subscription_expiry,
            r.subscription_signature,
        )
    })
    .map_err(|e| {
        if e.is_connection() {
            plugin
                .state()
                .lock()
                .unwrap()
                .set_tower_status(tower_id, TowerStatus::TemporaryUnreachable);
        }
        to_cln_error(e)
    })?;

    if !receipt.verify(&tower_id) {
        return Err(anyhow!(
            "Registration receipt contains bad signature. Are you using the right tower_id?"
        ));
    }

    plugin
        .state()
        .lock()
        .unwrap()
        .add_update_tower(tower_id, tower_net_addr, &receipt).map_err(|e| {
            if e.is_expiry() {
                anyhow!("Registration receipt contains a subscription expiry that is not higher than the one we are currently registered for")
            } else {
                anyhow!("Registration receipt does not contain more slots than the ones we are currently registered for")
            }
        })?;

    log::info!(
        "Registration succeeded. Available slots: {}. Subscription period (block height range): ({}-{})",
        receipt.available_slots(),
        receipt.subscription_start(),
        receipt.subscription_expiry()
    );

    Ok(json!(receipt))
}

/// Gets the latest registration receipt from the client to a given tower (if it exists).
///
/// This is pulled from the database
async fn get_registration_receipt(
    plugin: Plugin<Arc<Mutex<WTClient>>>,
    v: serde_json::Value,
) -> Result<serde_json::Value, Error> {
    let tower_id = TowerId::try_from(v).map_err(|x| anyhow!(x))?;
    let state = plugin.state().lock().unwrap();

    let response = state.get_registration_receipt(tower_id).map_err(|_| {
        anyhow!(
            "Cannot find {} within the known towers. Have you registered?",
            tower_id
        )
    })?;

    Ok(json!(response))
}

/// Gets information about an appointment from the tower.
async fn get_appointment(
    plugin: Plugin<Arc<Mutex<WTClient>>>,
    v: serde_json::Value,
) -> Result<serde_json::Value, Error> {
    let params = GetAppointmentParams::try_from(v).map_err(|x| anyhow!(x))?;

    let user_sk = plugin.state().lock().unwrap().user_sk;
    let tower_net_addr = {
        let state = plugin.state().lock().unwrap();
        if let Some(info) = state.towers.get(&params.tower_id) {
            Ok(info.net_addr.clone())
        } else {
            Err(anyhow!("Unknown tower id: {}", params.tower_id))
        }
    }?;

    let get_appointment_endpoint = format!("{}/get_appointment", tower_net_addr);
    let signature = cryptography::sign(
        format!("get appointment {}", params.locator).as_bytes(),
        &user_sk,
    )
    .unwrap();

    let response: ApiResponse<common_msgs::GetAppointmentResponse> = process_post_response(
        post_request(
            &get_appointment_endpoint,
            &common_msgs::GetAppointmentRequest {
                locator: params.locator.to_vec(),
                signature,
            },
        )
        .await,
    )
    .await
    .map_err(|e| {
        if e.is_connection() {
            plugin
                .state()
                .lock()
                .unwrap()
                .set_tower_status(params.tower_id, TowerStatus::TemporaryUnreachable);
        }
        to_cln_error(e)
    })?;

    Ok(json!(response))
}

/// Gets an appointment receipt from the client given a tower_id and a locator (if it exists).
///
/// This is pulled from the database
async fn get_appointment_receipt(
    plugin: Plugin<Arc<Mutex<WTClient>>>,
    v: serde_json::Value,
) -> Result<serde_json::Value, Error> {
    let params = GetAppointmentParams::try_from(v).map_err(|x| anyhow!(x))?;
    let state = plugin.state().lock().unwrap();

    let response = state
        .get_appointment_receipt(params.tower_id, params.locator)
        .map_err(|_| {
            if state.towers.contains_key(&params.tower_id) {
                anyhow!(
                    "Cannot find {} within {}. Did you send that appointment?",
                    params.locator,
                    params.tower_id
                )
            } else {
                anyhow!(
                    "Cannot find {} within the known towers. Have you registered?",
                    params.tower_id
                )
            }
        })?;

    Ok(json!(response))
}

/// Lists all the registered towers.
///
/// The given information comes from memory, so it is summarized.
async fn list_towers(
    plugin: Plugin<Arc<Mutex<WTClient>>>,
    _: serde_json::Value,
) -> Result<serde_json::Value, Error> {
    Ok(json!(plugin.state().lock().unwrap().towers))
}

/// Gets information about a given tower.
///
/// Data comes from disk (DB), so all stored data is provided.
async fn get_tower_info(
    plugin: Plugin<Arc<Mutex<WTClient>>>,
    v: serde_json::Value,
) -> Result<serde_json::Value, Error> {
    let state = plugin.state().lock().unwrap();
    let tower_id = TowerId::try_from(v).map_err(|e| anyhow!(e))?;
    let tower_info = state.load_tower_info(tower_id).map_err(|_| {
        anyhow!(
            "Cannot find {} within the known towers. Have you registered?",
            tower_id
        )
    })?;

    // Notice we need to check the status in memory since we cannot distinguish between unreachable and temporary unreachable
    // by just checking the data in the database.
    Ok(json!(
        tower_info.with_status(state.towers.get(&tower_id).unwrap().status)
    ))
}

/// Triggers a manual retry of a tower, tries to send all pending appointments to it.
///
/// Only works if the tower is unreachable or there's been a subscription error.
async fn retry_tower(
    plugin: Plugin<Arc<Mutex<WTClient>>>,
    v: serde_json::Value,
) -> Result<serde_json::Value, Error> {
    let tower_id = TowerId::try_from(v).map_err(|e| anyhow!(e))?;
    let state = plugin.state().lock().unwrap();
    if let Some(tower) = state.towers.get(&tower_id) {
        if tower.status == TowerStatus::TemporaryUnreachable {
            return Err(anyhow!("{} is already being retried", tower_id));
        } else if tower.status != TowerStatus::Unreachable {
            return Err(anyhow!(
                "Tower status must be unreachable to manually retry",
            ));
        }

        state
            .unreachable_towers
            .send(tower_id)
            .map_err(|e| anyhow!(e))?;
        Ok(json!(format!("Retrying {}", tower_id)))
    } else {
        Err(anyhow!("Unknown tower {}", tower_id))
    }
}

/// Forgets about a tower wiping out all local data associated to it.
async fn abandon_tower(
    plugin: Plugin<Arc<Mutex<WTClient>>>,
    v: serde_json::Value,
) -> Result<serde_json::Value, Error> {
    let tower_id = TowerId::try_from(v).map_err(|e| anyhow!(e))?;
    let mut state = plugin.state().lock().unwrap();
    if let Some(tower) = state.towers.get_mut(&tower_id) {
        if tower.status == TowerStatus::TemporaryUnreachable {
            tower.status = TowerStatus::Abandoned;
            return Err(anyhow!(
                "{} is being retried. Flagging it to be abandoned",
                tower_id
            ));
        }

        state.remove_tower(tower_id).unwrap();
        Ok(json!(format!("{} successfully abandoned", tower_id)))
    } else {
        Err(anyhow!("Unknown tower {}", tower_id))
    }
}

/// Sends an appointment to all registered towers for every new commitment transaction.
///
/// The appointment is built using the data provided by the backend (dispute txid and penalty transaction).
async fn on_commitment_revocation(
    plugin: Plugin<Arc<Mutex<WTClient>>>,
    v: serde_json::Value,
) -> Result<serde_json::Value, Error> {
    let commitment_revocation = serde_json::from_value::<CommitmentRevocation>(v)
        .map_err(|e| anyhow!("Cannot decode commitment_revocation data. Error: {}", e))?;
    log::debug!(
        "New commitment revocation received for channel {}. Commit number {}",
        commitment_revocation.channel_id,
        commitment_revocation.commit_num
    );

    // TODO: For now, to_self_delay is hardcoded to 42. Revisit and define it better / remove it when / if needed
    let locator = Locator::new(commitment_revocation.commitment_txid);
    let appointment = Appointment::new(
        locator,
        cryptography::encrypt(
            &commitment_revocation.penalty_tx,
            &commitment_revocation.commitment_txid,
        )
        .unwrap(),
        42,
    );
    let signature = cryptography::sign(
        &appointment.to_vec(),
        &plugin.state().lock().unwrap().user_sk,
    )
    .unwrap();

    // Looks like we cannot iterate through towers given a locked state is not Send (due to the async call),
    // so we need to clone the bare minimum.
    let towers = plugin
        .state()
        .lock()
        .unwrap()
        .towers
        .iter()
        .map(|(id, info)| (*id, info.net_addr.clone(), info.status))
        .collect::<Vec<_>>();

    for (tower_id, net_addr, status) in towers {
        if status.is_reachable() {
            match add_appointment(tower_id, &net_addr, &appointment, &signature).await {
                Ok((slots, receipt)) => {
                    plugin
                        .state()
                        .lock()
                        .unwrap()
                        .add_appointment_receipt(tower_id, locator, slots, &receipt);
                    log::debug!("Response verified and data stored in the database");
                }
                Err(e) => match e {
                    AddAppointmentError::RequestError(e) => {
                        if e.is_connection() {
                            log::warn!(
                                "{} cannot be reached. Adding {} to pending appointments",
                                tower_id,
                                appointment.locator
                            );
                            let mut state = plugin.state().lock().unwrap();
                            state.set_tower_status(tower_id, TowerStatus::TemporaryUnreachable);
                            state.add_pending_appointment(tower_id, &appointment);

                            state.unreachable_towers.send(tower_id).unwrap();
                        }
                    }
                    AddAppointmentError::ApiError(e) => match e.error_code {
                        errors::INVALID_SIGNATURE_OR_SUBSCRIPTION_ERROR => {
                            log::warn!("There is a subscription issue with {}", tower_id);
                            let mut state = plugin.state().lock().unwrap();
                            state.set_tower_status(tower_id, TowerStatus::SubscriptionError);
                            state.add_pending_appointment(tower_id, &appointment);

                            state.unreachable_towers.send(tower_id).unwrap();
                        }

                        _ => {
                            log::warn!(
                                "{} rejected the appointment. Error: {}, error_code: {}",
                                tower_id,
                                e.error,
                                e.error_code
                            );
                            plugin
                                .state()
                                .lock()
                                .unwrap()
                                .add_invalid_appointment(tower_id, &appointment);
                        }
                    },
                    AddAppointmentError::SignatureError(proof) => {
                        log::warn!("Cannot recover known tower_id from the appointment receipt. Flagging tower as misbehaving");
                        plugin
                            .state()
                            .lock()
                            .unwrap()
                            .flag_misbehaving_tower(tower_id, proof)
                    }
                },
            };
        } else if status.is_misbehaving() {
            log::warn!(
                "{} is misbehaving. Not sending any further appointments",
                tower_id
            );
        } else {
            if status.is_subscription_error() {
                log::warn!(
                    "There is a subscription issue with {}. Adding appointment to pending",
                    tower_id,
                );
            } else {
                log::warn!("{} is {}. Adding appointment to pending", tower_id, status);
            }

            plugin
                .state()
                .lock()
                .unwrap()
                .add_pending_appointment(tower_id, &appointment);
        }
    }

    // FIXME: Ask cdecker: Do hooks need to return something?
    Ok(json!(r#" {"result": continue}"#))
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let data_dir = match env::var("TOWERS_DATA_DIR") {
        Ok(v) => PathBuf::from(v),
        Err(_) => home_dir().unwrap().join(".watchtower"),
    };

    let (tx, rx) = unbounded_channel();
    let state = Arc::new(Mutex::new(WTClient::new(data_dir, tx).await));
    let state_clone = state.clone();

    let builder = Builder::new(state, stdin(), stdout())
        .option(ConfigOption::new(
            "watchtower-port",
            Value::Integer(9814),
            "tower API port",
        ))
        .option(ConfigOption::new(
            "watchtower-max-retry-time",
            Value::Integer(900),
            "the time (in seconds) after where the retrier will give up trying to send data to a temporary unreachable tower",
        ))
        .option(ConfigOption::new(
            "dev-watchtower-max-retry-interval",
            Value::Integer(60),
            "the maximum time (in seconds) for a retrier wait interval",
        ))
        .rpcmethod(
            "registertower",
            "Registers the client public key (user id) with the tower.",
            register,
        ).rpcmethod(
            "getregistrationreceipt",
            "Gets the latest registration receipt given a tower id.",
            get_registration_receipt,
        )
        .rpcmethod(
            "getappointment",
            "Gets appointment data from the tower given the tower id and the locator.",
            get_appointment,
        ).rpcmethod(
            "getappointmentreceipt",
            "Gets a (local) appointment receipt given a tower id and an locator.",
            get_appointment_receipt,
        )
        .rpcmethod("listtowers", "Lists all registered towers.", list_towers)
        .rpcmethod(
            "gettowerinfo",
            "Shows the info about a given tower.",
            get_tower_info,
        )
        .rpcmethod(
            "retrytower",
            "Retries to send pending appointment to an unreachable tower.",
            retry_tower,
        )
        .rpcmethod(
            "abandontower",
            "Forgets about a tower and wipes all local data.",
            abandon_tower,
        )
        .hook("commitment_revocation", on_commitment_revocation);

    if let Some(plugin) = builder.start().await? {
        // FIXME: This is a workaround. Ideally, `cln_plugin::options::Value` will implement `as_u64` so we can simply call and unwrap
        // given that we are certain the option exists.
        let max_elapsed_time =
            if let Value::Integer(x) = plugin.option("watchtower-max-retry-time").unwrap() {
                x as u16
            } else {
                // We will never end up here, but we need to define an else. Should be fixed alongside the previous fixme.
                900
            };
        let max_interval_time = if let Value::Integer(x) =
            plugin.option("dev-watchtower-max-retry-interval").unwrap()
        {
            x as u16
        } else {
            // We will never end up here, but we need to define an else. Should be fixed alongside the previous fixme.
            60
        };
        tokio::spawn(async move {
            Retrier::new(state_clone, max_elapsed_time, max_interval_time)
                .manage_retry(rx)
                .await
        });
        plugin.join().await
    } else {
        Ok(())
    }
}
