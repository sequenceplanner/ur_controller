use futures::stream::Stream;
use futures::StreamExt;
use r2r::geometry_msgs::msg::TransformStamped;
use r2r::scene_manipulation_msgs::srv::LookupTransform;
use r2r::sensor_msgs::msg::JointState;
use r2r::simple_robot_simulator_msgs::action::SimpleRobotControl;
use r2r::ur_controller_msgs::action::URControl;
use r2r::ur_controller_msgs::msg::Payload;
use r2r::ur_controller_msgs::srv::GenerateURScript;
use r2r::ur_script_msgs::action::ExecuteScript;
use r2r::ActionServerGoal;
use r2r::ParameterValue;
use serde::{Deserialize, Serialize};
use tera;

pub static NODE_ID: &'static str = "ur_controller";
pub static BASEFRAME_ID: &'static str = "base_link";
pub static FACEPLATE_ID: &'static str = "tool0";

#[derive(Serialize, Deserialize)]
pub struct Interpretation {
    pub command: String,
    pub acceleration: f64,
    pub velocity: f64,
    pub use_execution_time: bool,
    pub execution_time: f32,
    pub use_blend_radius: bool,
    pub blend_radius: f32,
    pub use_joint_positions: bool,
    pub joint_positions: String,
    pub use_preferred_joint_config: bool,
    pub preferred_joint_config: String,
    pub use_payload: bool,
    pub payload: String,
    pub target_in_base: String,
    pub tcp_in_faceplate: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ctx = r2r::Context::create()?;
    let mut node = r2r::Node::create(ctx, NODE_ID, "")?;

    // handle parameters passed on from the launch files
    let params = node.params.clone();
    let params_things = params.lock().unwrap(); // OK to panic
    let simple_param = params_things.get("simple_robot_simulator");
    let prefix_param = params_things.get("prefix");

    let simple = match simple_param {
        Some(p) => match p {
            ParameterValue::Bool(value) => *value,
            _ => {
                r2r::log_warn!(
                    NODE_ID,
                    "Parameter 'simple_robot_simulator' has to be of type Bool. The Simple Robot Simulation will be used."
                );
                true
            }
        },
        None => {
            r2r::log_warn!(
                NODE_ID,
                "Parameter 'simple_robot_simulator' not specified. The Simple Robot Simulation will be used."
            );
            true
        }
    };

    let prefix = match prefix_param {
        Some(p) => match p {
            ParameterValue::String(value) => value.clone(),
            _ => "".to_string()
        }
        None => "".to_string()
    };

    let action = node.create_action_server::<URControl::Action>("ur_control")?;

    match simple {
        true => {
            let simple_robot_simulator_client =
                node.create_action_client::<SimpleRobotControl::Action>("simple_robot_control")?;
            let waiting_for_simple_server = node.is_available(&simple_robot_simulator_client)?;

            let handle = std::thread::spawn(move || loop {
                node.spin_once(std::time::Duration::from_millis(100));
            });

            r2r::log_warn!(NODE_ID, "Waiting for the Simple Robot Simulator Service...");
            waiting_for_simple_server.await?;
            r2r::log_info!(NODE_ID, "Simple Robot Simulator available.");

            tokio::task::spawn(async move {
                let result = simple_controller_server(action, simple_robot_simulator_client, &prefix).await;
                match result {
                    Ok(()) => r2r::log_info!(NODE_ID, "Simple Controller Service call succeeded."),
                    Err(e) => r2r::log_error!(
                        NODE_ID,
                        "Simple Controller Service call failed with: {}.",
                        e
                    ),
                };
            });

            handle.join().unwrap();
        }
        false => {
            let templates_path_param = params_things.get("templates_path");

            let templates_path = match templates_path_param {
                Some(p) => match p {
                    ParameterValue::String(value) => value.clone(),
                    _ => {
                        r2r::log_error!(
                            NODE_ID,
                            "Parameter 'templates_path' has to be of type String."
                        );
                        "".to_string()
                    }
                },
                None => {
                    r2r::log_error!(NODE_ID, "Parameter 'templates_path' not specified!");
                    "".to_string()
                }
            };

            let templates: tera::Tera = {
                let tera = match tera::Tera::new(&format!("{}/templates/*.script", templates_path))
                {
                    Ok(t) => {
                        r2r::log_warn!(NODE_ID, "Searching for Tera templates, wait...",);
                        t
                    }
                    Err(e) => {
                        r2r::log_error!(NODE_ID, "UR Script template parsing error(s): {}", e);
                        ::std::process::exit(1);
                    }
                };
                tera
            };

            let template_names = templates
                .get_template_names()
                .map(|x| x.to_string())
                .collect::<Vec<String>>();
            if template_names.len() == 0 {
                r2r::log_error!(NODE_ID, "Couldn't find any Tera templates.");
            }
            for template in &template_names {
                r2r::log_info!(NODE_ID, "Found template: {:?}", template);
            }

            let urc_client = node.create_action_client::<ExecuteScript::Action>("ur_script")?;
            let waiting_urc_server = node.is_available(&urc_client)?;

            let tf_lookup_client =
                node.create_client::<LookupTransform::Service>("lookup_transform")?;
            let waiting_for_tf_lookup_server = node.is_available(&tf_lookup_client)?;

            let handle = std::thread::spawn(move || loop {
                node.spin_once(std::time::Duration::from_millis(100));
            });

            r2r::log_warn!(NODE_ID, "Waiting for the URScript Driver Service...");
            waiting_urc_server.await?;
            r2r::log_info!(NODE_ID, "URScript Driver available.");

            r2r::log_warn!(NODE_ID, "Waiting for tf Lookup service...");
            waiting_for_tf_lookup_server.await?;
            r2r::log_info!(NODE_ID, "tf Lookup Service available.");

            tokio::task::spawn(async move {
                let result =
                    urscript_controller_server(action, &urc_client, &tf_lookup_client, &templates)
                        .await;
                match result {
                    Ok(()) => r2r::log_info!(NODE_ID, "URScript Driver Service call succeeded."),
                    Err(e) => {
                        r2r::log_error!(NODE_ID, "URScript Driver Service call failed with: {}.", e)
                    }
                };
            });

            handle.join().unwrap();
        }
    }

    r2r::log_warn!(NODE_ID, "Node started.");

    Ok(())
}

async fn simple_controller_server(
    mut requests: impl Stream<Item = r2r::ActionServerGoalRequest<URControl::Action>> + Unpin,
    srs_client: r2r::ActionClient<SimpleRobotControl::Action>,
    prefix: &str
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        match requests.next().await {
            Some(request) => {
                let (mut g, mut _cancel) =
                    request.accept().expect("Could not accept goal request.");
                let g_clone = g.clone();
                match execute_simple_simulation(g_clone, &srs_client, prefix).await {
                    Ok(()) => {
                        g.succeed(URControl::Result { success: true })
                            .expect("Could not send result.");
                        continue;
                    }
                    Err(e) => {
                        let _ = g.abort(URControl::Result { success: false });
                        continue;
                    }
                }
            }
            None => (),
        }
    }
}

async fn urscript_controller_server(
    mut requests: impl Stream<Item = r2r::ActionServerGoalRequest<URControl::Action>> + Unpin,
    urc_client: &r2r::ActionClient<ExecuteScript::Action>,
    tf_lookup_client: &r2r::Client<LookupTransform::Service>,
    templates: &tera::Tera,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        match requests.next().await {
            Some(request) => {
                let (mut g, mut _cancel) =
                    request.accept().expect("Could not accept goal request.");
                let g_clone = g.clone();
                match execute_urscript(g_clone, &urc_client, &tf_lookup_client, &templates).await {
                    Ok(ok) => {
                        g.succeed(URControl::Result { success: ok })
                            .expect("Could not send result.");
                        continue;
                    }
                    Err(e) => {
                        let _ = g.abort(URControl::Result { success: false });
                        continue;
                    }
                }
            }
            None => (),
        }
    }
}

fn urc_goal_to_srs_goal(urc_goal: URControl::Goal, prefix: &str) -> SimpleRobotControl::Goal {
    SimpleRobotControl::Goal {
        base_frame_id: format!("{}{}", prefix, BASEFRAME_ID.to_string()), // BASEFRAME_ID.to_string()
        face_plate_id: format!("{}{}", prefix, FACEPLATE_ID.to_string()), // FACEPLATE_ID.to_string(),
        tcp_id: urc_goal.tcp_id,
        goal_feature_id: urc_goal.goal_feature_id,
        acceleration: urc_goal.acceleration,
        velocity: urc_goal.velocity,
        use_joint_positions: urc_goal.use_joint_positions,
        joint_positions: urc_goal.joint_positions,
    }
}

async fn execute_simple_simulation(
    g: ActionServerGoal<URControl::Action>,
    srs_client: &r2r::ActionClient<SimpleRobotControl::Action>,
    prefix: &str
) -> Result<(), Box<dyn std::error::Error>> {
    let goal = urc_goal_to_srs_goal(g.goal.clone(), prefix);

    r2r::log_info!(NODE_ID, "Sending request to Simple Robot Simulator.");
    let _ = g.publish_feedback(URControl::Feedback {
        current_state: "Sending request to Simple Robot Simulator.".into(),
    });

    let (_goal, result, _feedback) = match srs_client.send_goal_request(goal) {
        Ok(x) => match x.await {
            Ok(y) => y,
            Err(e) => {
                r2r::log_info!(NODE_ID, "Could not send goal request.");
                return Err(Box::new(e));
            }
        },
        Err(e) => {
            r2r::log_info!(NODE_ID, "Did not get goal.");
            return Err(Box::new(e));
        }
    };

    match result.await {
        Ok((status, msg)) => match status {
            r2r::GoalStatus::Aborted => {
                r2r::log_info!(NODE_ID, "Goal succesfully aborted with: {:?}", msg);
                let _ = g.publish_feedback(URControl::Feedback {
                    current_state: "Goal succesfully aborted.".into(),
                });
                Ok(())
            }
            _ => {
                r2r::log_info!(
                    NODE_ID,
                    "Executing the Simple Robot Simulator action succeeded."
                );
                let _ = g.publish_feedback(URControl::Feedback {
                    current_state: "Executing the Simple Robot Simulator action succeeded.".into(),
                });
                Ok(())
            }
        },
        Err(e) => {
            r2r::log_error!(
                NODE_ID,
                "Simple Robot Simulator action failed with: {:?}",
                e,
            );
            let _ = g.publish_feedback(URControl::Feedback {
                current_state: "Simple Robot Simulator action failed. Aborting.".into(),
            });
            return Err(Box::new(e));
        }
    }
}

async fn execute_urscript(
    g: ActionServerGoal<URControl::Action>,
    urc_client: &r2r::ActionClient<ExecuteScript::Action>,
    tf_lookup_client: &r2r::Client<LookupTransform::Service>,
    templates: &tera::Tera,
) -> Result<bool, Box<dyn std::error::Error>> {
    let goal = match generate_script(g.goal.clone(), &tf_lookup_client, &templates).await {
        Some(script) => ExecuteScript::Goal { script },
        None => return Ok(false), // RETURN ERROR SOMEHOW: Err(std::error::Error::default())
    };

    r2r::log_info!(NODE_ID, "Sending request to UR Script Driver.");
    let _ = g.publish_feedback(URControl::Feedback {
        current_state: "Sending request to UR Script Driver.".into(),
    });

    let (_goal, result, _feedback) = match urc_client.send_goal_request(goal) {
        Ok(x) => match x.await {
            Ok(y) => y,
            Err(e) => {
                r2r::log_info!(NODE_ID, "Could not send goal request.");
                return Err(Box::new(e));
            }
        },
        Err(e) => {
            r2r::log_info!(NODE_ID, "Did not get goal.");
            return Err(Box::new(e));
        }
    };

    match result.await {
        Ok((status, msg)) => match status {
            r2r::GoalStatus::Aborted => {
                r2r::log_info!(NODE_ID, "Goal succesfully aborted with: {:?}", msg);
                let _ = g.publish_feedback(URControl::Feedback {
                    current_state: "Goal succesfully aborted.".into(),
                });
                Ok(false)
            }
            _ => {
                r2r::log_info!(NODE_ID, "Executing the UR Script succeeded? {}", msg.ok);
                let _ = g.publish_feedback(URControl::Feedback {
                    current_state: "Executing the UR Script succeeded.".into(),
                });
                Ok(msg.ok)
            }
        },
        Err(e) => {
            r2r::log_error!(NODE_ID, "UR Script Driver Action failed with: {:?}", e,);
            let _ = g.publish_feedback(URControl::Feedback {
                current_state: "UR Script Driver Action failed. Aborting.".into(),
            });
            return Err(Box::new(e));
        }
    }
}

async fn generate_script(
    message: URControl::Goal,
    tf_lookup_client: &r2r::Client<LookupTransform::Service>,
    templates: &tera::Tera,
) -> Option<String> {
    let empty_context = tera::Context::new();
    let interpreted_message = interpret_message(&message, &tf_lookup_client).await;
    match templates.render(
        &format!("{}.script", message.command),
        match &tera::Context::from_serialize(interpreted_message) {
            Ok(context) => context,
            Err(e) => {
                r2r::log_error!(
                    NODE_ID,
                    "Creating a Tera Context from a serialized Interpretation failed with: {}.",
                    e
                );
                r2r::log_warn!(NODE_ID, "An empty Tera Context will be used instead.");
                &empty_context
            }
        },
    ) {
        Ok(script) => Some(script),
        Err(e) => {
            r2r::log_error!(
                NODE_ID,
                "Rendering the {}.script Tera Template failed with: {}.",
                message.command,
                e
            );
            None
        }
    }
}

async fn interpret_message(
    message: &URControl::Goal,
    tf_lookup_client: &r2r::Client<LookupTransform::Service>,
) -> Option<Interpretation> {
    let target_in_base = match message.use_joint_positions && message.command == "move_j" {
        true => pose_to_string(&TransformStamped::default()),
        false => match lookup_tf(BASEFRAME_ID, &message.goal_feature_id, tf_lookup_client).await {
            Some(transform) => pose_to_string(&transform),
            None => return None,
        },
    };

    let tcp_in_faceplate = match message.use_joint_positions && message.command == "move_j" {
        true => pose_to_string(&TransformStamped::default()),
        false => match lookup_tf(FACEPLATE_ID, &message.tcp_id, tf_lookup_client).await {
            Some(transform) => pose_to_string(&transform),
            None => return None,
        },
    };

    Some(Interpretation {
        command: message.command.to_string(),
        acceleration: message.acceleration,
        velocity: message.velocity,
        use_execution_time: message.use_execution_time,
        execution_time: message.execution_time,
        use_blend_radius: message.use_blend_radius,
        blend_radius: message.blend_radius,
        use_joint_positions: message.use_joint_positions,
        joint_positions: joint_pose_to_string(message.joint_positions.clone()),
        use_preferred_joint_config: message.use_preferred_joint_config,
        preferred_joint_config: joint_pose_to_string(message.preferred_joint_config.clone()),
        use_payload: message.use_payload,
        payload: payload_to_string(message.payload.clone()),
        target_in_base,
        tcp_in_faceplate,
    })
}

fn payload_to_string(p: Payload) -> String {
    format!(
        "{},[{},{},{}],[{},{},{},{},{},{}]",
        p.mass, p.cog_x, p.cog_y, p.cog_z, p.ixx, p.iyy, p.izz, p.ixy, p.ixz, p.iyz
    )
}

fn joint_pose_to_string(j: JointState) -> String {
    match j.position.len() == 6 {
        true => format!(
            "[{},{},{},{},{},{}]",
            j.position[0],
            j.position[1],
            j.position[2],
            j.position[3],
            j.position[4],
            j.position[5]
        ),
        false => "".to_string(),
    }
}

fn pose_to_string(tf_stamped: &TransformStamped) -> String {
    let x = tf_stamped.transform.translation.x;
    let y = tf_stamped.transform.translation.y;
    let z = tf_stamped.transform.translation.z;
    let rot = tf_stamped.transform.rotation.clone();
    let angle = 2.0 * rot.w.acos();
    let den = (1.0 - rot.w.powi(2)).sqrt();
    let (rx, ry, rz) = match den < 0.001 {
        true => (rot.x * angle, rot.y * angle, rot.z * angle),
        false => (
            (rot.x / den) * angle,
            (rot.y / den) * angle,
            (rot.z / den) * angle,
        ),
    };
    format!("p[{},{},{},{},{},{}]", x, y, z, rx, ry, rz)
}

// ask the lookup service for transforms from its buffer
async fn lookup_tf(
    parent_frame_id: &str,
    child_frame_id: &str,
    // deadline: i32,
    tf_lookup_client: &r2r::Client<LookupTransform::Service>,
) -> Option<TransformStamped> {
    let request = LookupTransform::Request {
        parent_frame_id: parent_frame_id.to_string(),
        child_frame_id: child_frame_id.to_string(),
        // deadline,
    };

    let response = tf_lookup_client
        .request(&request)
        .expect("Could not send tf Lookup request.")
        .await
        .expect("Cancelled.");

    r2r::log_info!(
        NODE_ID,
        "Request to lookup parent '{}' to child '{}' sent.",
        parent_frame_id,
        child_frame_id
    );

    match response.success {
        true => Some(response.transform),
        false => {
            r2r::log_error!(
                NODE_ID,
                "Couldn't lookup tf for parent '{}' and child '{}'.",
                parent_frame_id,
                child_frame_id
            );
            None
        }
    }
}
