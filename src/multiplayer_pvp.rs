#![allow(unused)]
use crate::backgrounds::{Tile, WIN_H, WIN_W};
use crate::camera::MultCamera;
use crate::game_client::{
    self, get_randomized_port, EnemyMonsterSpawned, GameClient, PlayerType, ReadyToSpawnEnemy,
};
use crate::monster::{
    get_monster_sprite_for_type, Boss, Defense, Element, Enemy, Health, Level, MonsterStats, Moves,
    PartyMonster, SelectedMonster, Strength,
};
use crate::multiplayer_waiting::{is_client, is_host};
use crate::networking::{
    BattleAction, BattleData, ClientActionEvent, HostActionEvent, Message, MonsterTypeEvent,
    MultBattleBackground, MultBattleUIElement, MultEnemyHealth, MultEnemyMonster, MultMonster,
    MultPlayerHealth, MultPlayerMonster, SelectedEnemyMonster, MULT_BATTLE_BACKGROUND,
};
use crate::world::{PooledText, TextBuffer, TypeSystem};
use crate::GameState;
use bevy::{prelude::*, ui::*};
use bincode;
use iyes_loopless::prelude::*;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::net::{Ipv4Addr, UdpSocket};
use std::str::from_utf8;
use std::{io, thread};

/// Flag to determine whether this
#[derive(Clone, Copy)]
pub(crate) struct TurnFlag(pub(crate) bool);

// impl FromWorld for TurnFlag {
//     fn from_world(world: &mut World) -> Self {
//         let player_type = world
//             .get_resource::<GameClient>()
//             .expect("unable to retrieve player type for initialization")
//             .player_type;

//         // Host gets to go first
//         Self(player_type == PlayerType::Host)
//     }
// }

// Host side:
// - Pick action 0-3 (based off of host's key_press_handler)
//   + Cache this action in a ActionCache(usize)
// - Send their action choice and stats to client in a BattleAction::StartTurn
//   + Flip TurnFlag to false
// - recv_packet receives BattleAction::FinishTurn which will contain the action the client took (0-3) and their stats
//   + Client stats will be stored via updating local entities
//   + Client's action will be sent to handler for FinishTurnEvent within the event
//   + They will be denied by keypress handler if they try to press again when not their turn
//   + Receiver will fire a FinishTurnEvent
// - handler function for FinishTurnEvent will run
//   + Queries ActionCache and uses the client action taken out of the event, both of which were setup prior
//   + Calculates local updates with `calculate_turn` and the above actions
//   + Updates stats locally based on result
//   + Flips TurnFlag to true
//
// Client side:
// - recv_packet receives BattleAction::StartTurn
//   + Caches action chosen by host in ActionCache(usize)
//   + This flips TurnFlag to true
// - Picks their own action (0-3) (with client version of key_press_handler)
//   + client_key_press_handler needs to query the resource ActionCache(usize) to
//     get the action out of the recv_packet system to do turn calculation
//   + They will be denied by keypress handler if not their turn)
//   + Sends a BattleAction::FinishTurn with their own action and stats
//   + Calculates the result with `calculate_turn`
//   + Updates stats locally based on result
//   + Flips TurnFlag to false

// turn(host): choose action, disable turn, send
// turn(client): choose action, disable turn, calculate result, send
// turn(host): calculate result, next turn...

#[derive(Component, Debug, Default)]
pub(crate) struct CachedData(BattleData);

#[derive(Component, Debug, Default)]
pub(crate) struct CachedAction(usize);

// Builds plugin for multiplayer battles
pub struct MultPvPPlugin;
impl Plugin for MultPvPPlugin {
    fn build(&self, app: &mut App) {
        app.add_enter_system_set(
            GameState::MultiplayerPvPBattle,
            SystemSet::new()
                .with_system(setup_mult_battle)
                .with_system(setup_mult_battle_stats)
                .with_system(init_host_turnflag.run_if(is_host))
                .with_system(init_client_turnflag.run_if(is_client)),
        )
        .add_system_set(
            ConditionSet::new()
                // Only run handlers on MultiplayerBattle state
                .run_in_state(GameState::MultiplayerPvPBattle)
                .with_system(spawn_mult_player_monster)
                .with_system(spawn_mult_enemy_monster.run_if_resource_exists::<ReadyToSpawnEnemy>())
                .with_system(update_mult_battle_stats)
                // .with_system(mult_key_press_handler.run_if_resource_exists::<EnemyMonsterSpawned>())
                .with_system(
                    host_action_handler
                        .run_if_resource_exists::<EnemyMonsterSpawned>()
                        .run_if_resource_exists::<TurnFlag>()
                        .run_if(is_host),
                )
                .with_system(
                    host_end_turn_handler
                        .run_if_resource_exists::<TurnFlag>()
                        .run_if(is_host),
                )
                .with_system(
                    client_action_handler
                        .run_if_resource_exists::<TurnFlag>()
                        .run_if(is_client),
                )
                .with_system(
                    client_end_turn_handler
                        .run_if_resource_exists::<TurnFlag>()
                        .run_if(is_client),
                )
                .with_system(recv_packets.run_if_resource_exists::<TurnFlag>())
                .with_system(handle_monster_type_event)
                .into(),
        )
        // Turn flag keeps track of whether or not it is our turn currently
        // GameClient resource has not been initialized at this point
        .init_resource::<CachedData>()
        .init_resource::<CachedAction>()
        .add_event::<HostActionEvent>()
        .add_event::<ClientActionEvent>()
        .add_exit_system(GameState::MultiplayerPvPBattle, despawn_mult_battle);
    }
}

pub(crate) fn init_host_turnflag(mut commands: Commands) {
    commands.insert_resource(TurnFlag(true));
}

pub(crate) fn init_client_turnflag(mut commands: Commands) {
    commands.insert_resource(TurnFlag(false));
}

pub(crate) fn recv_packets(
    game_client: Res<GameClient>,
    mut commands: Commands,
    mut monster_type_event: EventWriter<MonsterTypeEvent>,
    mut host_action_event: EventWriter<HostActionEvent>,
    mut player_monster: Query<
        (&mut Health, &mut Strength, &mut Defense, Entity, &Element),
        (With<SelectedMonster>),
    >,
    mut enemy_monster: Query<
        (&mut Health, &mut Strength, &mut Defense, Entity, &Element),
        (Without<SelectedMonster>, With<SelectedEnemyMonster>),
    >,
    mut turn: ResMut<TurnFlag>,
    mut battle_data: ResMut<CachedData>,
    mut text_buffer: ResMut<TextBuffer>,
) {
    loop {
        let mut buf = [0; 512];
        match game_client.socket.udp_socket.recv(&mut buf) {
            Ok(msg) => {
                //info!("from here: {}, {:#?}", msg, &buf[..msg]);
                let deserialized_msg: Message = bincode::deserialize(&buf[..msg]).unwrap();
                let action_type = deserialized_msg.action.clone();
                info!("Action type: {:#?}", action_type);
                info!("Payload is: {:?}", deserialized_msg.payload.clone());

                if action_type == BattleAction::MonsterType {
                    let payload =
                        usize::from_ne_bytes(deserialized_msg.payload.clone().try_into().unwrap());
                    monster_type_event.send(MonsterTypeEvent {
                        message: deserialized_msg.clone(),
                    });
                } else if action_type == BattleAction::StartTurn {
                    // info!("Payload is: {:?}", deserialized_msg.payload.clone());
                    turn.0 = true;
                    let text = PooledText {
                        text: format!("Your turn!"),
                        pooled: false,
                    };
                    text_buffer.bottom_text.push_back(text);
                    let payload = deserialized_msg.payload.clone();
                    battle_data.0 = BattleData {
                        act: (payload[0]),
                        atk: (payload[1]),
                        crt: (payload[2]),
                        def: (payload[3]),
                        ele: (payload[4]),
                    };
                } else if action_type == BattleAction::FinishTurn {
                    turn.0 = true;
                    let text = PooledText {
                        text: format!("Your turn!"),
                        pooled: false,
                    };
                    text_buffer.bottom_text.push_back(text);
                    let payload = deserialized_msg.payload.clone();
                    host_action_event.send(HostActionEvent(BattleData {
                        act: (payload[0]),
                        atk: (payload[1]),
                        crt: (payload[2]),
                        def: (payload[3]),
                        ele: (payload[4]),
                    }));
                } else {
                    warn!("Unrecognized action type");
                    break;
                }
            }
            Err(err) => {
                if err.kind() != io::ErrorKind::WouldBlock {
                    // An ACTUAL error occurred
                    error!("{}", err);
                }

                break;
            }
        }
    }
}

fn client_action_handler(
    mut commands: Commands,
    input: Res<Input<KeyCode>>,
    mut client_monster_query: Query<
        (&mut Health, &mut Strength, &mut Defense, Entity, &Element),
        (With<SelectedMonster>),
    >,
    mut turn: ResMut<TurnFlag>,
    game_client: Res<GameClient>,
    mut battle_data: ResMut<CachedData>,
    mut client_action_event: EventWriter<ClientActionEvent>,
    mut client_cached_action: ResMut<CachedAction>,
    mut text_buffer: ResMut<TextBuffer>,
) {
    if client_monster_query.is_empty() {
        error!("client cannot find monster.");
        return;
    }

    // info!("client flag status: {:?}", turn.0);

    let (client_hp, client_stg, client_def, client_entity, client_element) =
        client_monster_query.single();

    // turn.0 accesses status of TurnFlag (what's in 0th index)
    if turn.0 == true {
        // This is client's turn
        if input.just_pressed(KeyCode::A) {
            turn.0 = false; // flip TurnFlag to false
            let mut action_and_data: Vec<u8> = Vec::new();
            action_and_data.push(0);
            action_and_data.push(client_stg.atk as u8);
            action_and_data.push(client_stg.crt as u8);
            action_and_data.push(client_def.def as u8);
            action_and_data.push(*client_element as u8);
            let msg = Message {
                action: BattleAction::FinishTurn,
                payload: action_and_data,
            };
            game_client
                .socket
                .udp_socket
                .send(&bincode::serialize(&msg).unwrap());

            client_action_event.send(ClientActionEvent(battle_data.0));
            client_cached_action.0 = 0;
        }
        if input.just_pressed(KeyCode::D) {
            turn.0 = false; // flip TurnFlag to false
            let mut action_and_data: Vec<u8> = Vec::new();
            action_and_data.push(1);
            action_and_data.push(client_stg.atk as u8);
            action_and_data.push(client_stg.crt as u8);
            action_and_data.push(client_def.def as u8);
            action_and_data.push(*client_element as u8);
            let msg = Message {
                action: BattleAction::FinishTurn,
                payload: action_and_data,
            };
            game_client
                .socket
                .udp_socket
                .send(&bincode::serialize(&msg).unwrap());

            client_action_event.send(ClientActionEvent(battle_data.0));
            client_cached_action.0 = 1;
        }
        if input.just_pressed(KeyCode::E) {
            turn.0 = false; // flip TurnFlag to false
            let mut action_and_data: Vec<u8> = Vec::new();
            action_and_data.push(2);
            action_and_data.push(client_stg.atk as u8);
            action_and_data.push(client_stg.crt as u8);
            action_and_data.push(client_def.def as u8);
            action_and_data.push(*client_element as u8);
            let msg = Message {
                action: BattleAction::FinishTurn,
                payload: action_and_data,
            };
            game_client
                .socket
                .udp_socket
                .send(&bincode::serialize(&msg).unwrap());

            client_action_event.send(ClientActionEvent(battle_data.0));
            client_cached_action.0 = 2;
        }
        if input.just_pressed(KeyCode::S) {
            turn.0 = false; // flip TurnFlag to false
            let mut action_and_data: Vec<u8> = Vec::new();
            action_and_data.push(3);
            action_and_data.push(client_stg.atk as u8);
            action_and_data.push(client_stg.crt as u8);
            action_and_data.push(client_def.def as u8);
            action_and_data.push(*client_element as u8);
            let msg = Message {
                action: BattleAction::FinishTurn,
                payload: action_and_data,
            };
            game_client
                .socket
                .udp_socket
                .send(&bincode::serialize(&msg).unwrap());

            client_action_event.send(ClientActionEvent(battle_data.0));
            client_cached_action.0 = 3;
        }
    }
}

pub(crate) fn client_end_turn_handler(
    mut commands: Commands,
    mut action_event: EventReader<ClientActionEvent>,
    mut client_cached_action: ResMut<CachedAction>,
    mut battle_data: ResMut<CachedData>,
    mut client_monster_query: Query<
        (&mut Health, &mut Strength, &mut Defense, Entity, &Element),
        (With<SelectedMonster>),
    >,
    mut enemy_monster_query: Query<
        (&mut Health, &mut Strength, &mut Defense, Entity, &Element),
        (Without<SelectedMonster>, With<SelectedEnemyMonster>),
    >,
    game_client: Res<GameClient>,
    type_system: Res<TypeSystem>,
    mut text_buffer: ResMut<TextBuffer>,
) {
    let mut wrapped_data: Option<BattleData> = None;
    for event in action_event.iter() {
        info!("Got action event");
        wrapped_data = Some(event.clone().0);
        info!("Client received data: {:?}", wrapped_data.unwrap());
    }

    if wrapped_data.is_none() {
        return;
    }

    let data = wrapped_data.unwrap();

    let (mut client_hp, client_stg, client_def, client_entity, client_element) =
        client_monster_query.single_mut();

    let (mut enemy_hp, enemy_stg, enemy_def, enemy_entity, enemy_element) =
        enemy_monster_query.single_mut();

    let turn_result = mult_calculate_turn(
        client_stg.atk as u8,
        client_stg.crt as u8,
        client_def.def as u8,
        *client_element as u8,
        client_cached_action.0 as u8,
        data.atk,
        data.crt,
        data.def,
        data.ele,
        data.act,
        *type_system,
    );

    info!("turn result: {:?}", turn_result);

    client_hp.health -= turn_result.1;
    enemy_hp.health -= turn_result.0;

    if (client_hp.health <= 0 && enemy_hp.health <= 0) {
        let text = PooledText {
            text: format!("Draw!"),
            pooled: false,
        };
        text_buffer.bottom_text.push_back(text);
        // TODO: Game over, return to main menu
        info!("Draw! Attemping to go to start screen...");
        commands.insert_resource(NextState(GameState::Start));
    } else if (client_hp.health <= 0) {
        let text = PooledText {
            text: format!("Player 1 (host) won!"),
            pooled: false,
        };
        text_buffer.bottom_text.push_back(text);
        info!("Player 1 (host) won!");
        commands.insert_resource(NextState(GameState::Start));
    } else if (enemy_hp.health <= 0) {
        let text = PooledText {
            text: format!("Player 2 (client) won!"),
            pooled: false,
        };
        text_buffer.bottom_text.push_back(text);
        info!("Player 2 (client) won!");
        commands.insert_resource(NextState(GameState::Start));
    }
}

fn handle_monster_type_event(
    mut monster_type_event_reader: EventReader<MonsterTypeEvent>,
    mut commands: Commands,
) {
    for ev in monster_type_event_reader.iter() {
        //info!("{:#?}", ev.message);
        let mut payload = usize::from_ne_bytes(ev.message.payload.clone().try_into().unwrap());

        // Create structs for opponent's monster
        let enemy_monster_stats = MonsterStats {
            typing: convert_num_to_element(payload),
            lvl: Level { level: 1 },
            hp: Health {
                max_health: 100,
                health: 100,
            },
            stg: Strength {
                atk: 2,
                crt: 25,
                crt_dmg: 2,
            },
            def: Defense {
                def: 1,
                crt_res: 10,
            },
            moves: Moves { known: 2 },
        };
        commands
            .spawn()
            .insert_bundle(enemy_monster_stats)
            .insert(SelectedEnemyMonster);

        commands.insert_resource(ReadyToSpawnEnemy {});
    }
}

fn host_action_handler(
    mut commands: Commands,
    input: Res<Input<KeyCode>>,
    mut host_monster_query: Query<
        (&mut Health, &mut Strength, &mut Defense, Entity, &Element),
        (With<SelectedMonster>),
    >,
    mut turn: ResMut<TurnFlag>,
    game_client: Res<GameClient>,
    mut battle_data: ResMut<CachedData>,
    mut host_cached_action: ResMut<CachedAction>,
    mut text_buffer: ResMut<TextBuffer>,
) {
    if host_monster_query.is_empty() {
        error!("Host cannot find monster.");
        return;
    }

    // info!("Host flag status: {:?}", turn.0);

    let (host_hp, host_stg, host_def, host_entity, host_element) = host_monster_query.single();

    // turn.0 accesses status of TurnFlag (what's in 0th index)
    if turn.0 == true {
        // This is host's turn
        // info!("Host may act");
        if input.just_pressed(KeyCode::A) {
            turn.0 = false; // flip TurnFlag to false
            let mut action_and_data: Vec<u8> = Vec::new();
            action_and_data.push(0);
            action_and_data.push(host_stg.atk as u8);
            action_and_data.push(host_stg.crt as u8);
            action_and_data.push(host_def.def as u8);
            action_and_data.push(*host_element as u8);
            let msg = Message {
                action: BattleAction::StartTurn,
                payload: action_and_data,
            };
            game_client
                .socket
                .udp_socket
                .send(&bincode::serialize(&msg).unwrap());

            battle_data.0 = BattleData {
                act: 0,
                atk: host_stg.atk as u8,
                crt: host_stg.crt as u8,
                def: host_def.def as u8,
                ele: *host_element as u8,
            }; //cache data

            host_cached_action.0 = 0;
        }
        if input.just_pressed(KeyCode::D) {
            turn.0 = false; // flip TurnFlag to false
            let mut action_and_data: Vec<u8> = Vec::new();
            action_and_data.push(1);
            action_and_data.push(host_stg.atk as u8);
            action_and_data.push(host_stg.crt as u8);
            action_and_data.push(host_def.def as u8);
            action_and_data.push(*host_element as u8);
            let msg = Message {
                action: BattleAction::StartTurn,
                payload: action_and_data,
            };
            game_client
                .socket
                .udp_socket
                .send(&bincode::serialize(&msg).unwrap());

            battle_data.0 = BattleData {
                act: 1,
                atk: host_stg.atk as u8,
                crt: host_stg.crt as u8,
                def: host_def.def as u8,
                ele: *host_element as u8,
            }; //cache data

            host_cached_action.0 = 1;
        }
        if input.just_pressed(KeyCode::E) {
            turn.0 = false; // flip TurnFlag to false
            let mut action_and_data: Vec<u8> = Vec::new();
            action_and_data.push(2);
            action_and_data.push(host_stg.atk as u8);
            action_and_data.push(host_stg.crt as u8);
            action_and_data.push(host_def.def as u8);
            action_and_data.push(*host_element as u8);
            let msg = Message {
                action: BattleAction::StartTurn,
                payload: action_and_data,
            };
            game_client
                .socket
                .udp_socket
                .send(&bincode::serialize(&msg).unwrap());

            battle_data.0 = BattleData {
                act: 2,
                atk: host_stg.atk as u8,
                crt: host_stg.crt as u8,
                def: host_def.def as u8,
                ele: *host_element as u8,
            }; //cache data

            host_cached_action.0 = 2;
        }
        if input.just_pressed(KeyCode::S) {
            turn.0 = false; // flip TurnFlag to false
            let mut action_and_data: Vec<u8> = Vec::new();
            action_and_data.push(3);
            action_and_data.push(host_stg.atk as u8);
            action_and_data.push(host_stg.crt as u8);
            action_and_data.push(host_def.def as u8);
            action_and_data.push(*host_element as u8);
            let msg = Message {
                action: BattleAction::StartTurn,
                payload: action_and_data,
            };
            game_client
                .socket
                .udp_socket
                .send(&bincode::serialize(&msg).unwrap());

            battle_data.0 = BattleData {
                act: 3,
                atk: host_stg.atk as u8,
                crt: host_stg.crt as u8,
                def: host_def.def as u8,
                ele: *host_element as u8,
            }; //cache data

            host_cached_action.0 = 3;
        }
    }
}

pub(crate) fn host_end_turn_handler(
    mut commands: Commands,
    mut action_event: EventReader<HostActionEvent>,
    mut turn: ResMut<TurnFlag>,
    mut host_monster_query: Query<
        (&mut Health, &mut Strength, &mut Defense, Entity, &Element),
        (With<SelectedMonster>),
    >,
    mut enemy_monster_query: Query<
        (&mut Health, &mut Strength, &mut Defense, Entity, &Element),
        (Without<SelectedMonster>, With<SelectedEnemyMonster>),
    >,
    game_client: Res<GameClient>,
    type_system: Res<TypeSystem>,
    cached_host_action: Res<CachedAction>,
    mut battle_data: ResMut<CachedData>,
    mut text_buffer: ResMut<TextBuffer>,
) {
    let mut wrapped_data: Option<BattleData> = None;
    for event in action_event.iter() {
        info!("Got action event");
        wrapped_data = Some(event.0);
        info!("Host received data: {:?}", wrapped_data.unwrap());
    }

    if wrapped_data.is_none() {
        return;
    }

    let data = wrapped_data.unwrap();

    let (mut host_hp, host_stg, host_def, host_entity, host_element) =
        host_monster_query.single_mut();

    let (mut enemy_hp, enemy_stg, enemy_def, enemy_entity, enemy_element) =
        enemy_monster_query.single_mut();

    let turn_result = mult_calculate_turn(
        host_stg.atk as u8,
        host_stg.crt as u8,
        host_def.def as u8,
        *host_element as u8,
        cached_host_action.0 as u8,
        data.atk,
        data.crt,
        data.def,
        data.ele,
        data.act,
        *type_system,
    );

    info!("turn result: {:?}", turn_result);

    host_hp.health -= turn_result.1;
    enemy_hp.health -= turn_result.0;

    if (host_hp.health <= 0 && enemy_hp.health <= 0) {
        let text = PooledText {
            text: format!("Draw!"),
            pooled: false,
        };
        text_buffer.bottom_text.push_back(text);
        // TODO: Game over, return to main menu
        info!("Draw! Attemping to go to start screen...");
        commands.insert_resource(NextState(GameState::Start));
    } else if (host_hp.health <= 0) {
        let text = PooledText {
            text: format!("Player 2 (client) won!"),
            pooled: false,
        };
        text_buffer.bottom_text.push_back(text);
        info!("Player 2 (client) won!");
        commands.insert_resource(NextState(GameState::Start));
    } else if (enemy_hp.health <= 0) {
        let text = PooledText {
            text: format!("Player 1 (host) won!"),
            pooled: false,
        };
        text_buffer.bottom_text.push_back(text);
        info!("Player 1 (host) won!");
        commands.insert_resource(NextState(GameState::Start));
    }
}

/// Take the type integer received and turn it into an actual Element
fn convert_num_to_element(num: usize) -> Element {
    match num {
        0 => Element::Scav,
        1 => Element::Growth,
        2 => Element::Ember,
        3 => Element::Flood,
        4 => Element::Rad,
        5 => Element::Robot,
        6 => Element::Clean,
        7 => Element::Filth,
        // Whar?
        _ => std::process::exit(256),
    }
}

pub(crate) fn setup_mult_battle(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    cameras: Query<Entity, (With<Camera2d>, Without<MultCamera>)>,
    game_client: Res<GameClient>,
    selected_monster_query: Query<(&Element), (With<SelectedMonster>)>,
) {
    cameras.for_each(|camera| {
        commands.entity(camera).despawn();
    });

    //creates camera for multiplayer battle background
    let camera = Camera2dBundle::default();
    commands.spawn_bundle(camera).insert(MultCamera);

    commands
        .spawn_bundle(SpriteBundle {
            texture: asset_server.load(MULT_BATTLE_BACKGROUND),
            transform: Transform::from_xyz(0., 0., 2.),
            ..default()
        })
        .insert(MultBattleBackground);

    // send type of monster to other player
    let (selected_type) = selected_monster_query.single();
    let num_type = *selected_type as usize;

    let msg = Message {
        action: BattleAction::MonsterType,
        payload: num_type.to_ne_bytes().to_vec(),
    };
    game_client
        .socket
        .udp_socket
        .send(&bincode::serialize(&msg).unwrap());
}

pub(crate) fn setup_mult_battle_stats(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    game_client: Res<GameClient>,
) {
    commands
        .spawn_bundle(
            // Create a TextBundle that has a Text with a list of sections.
            TextBundle::from_sections([
                // health header for player's monster
                TextSection::new(
                    "Health:",
                    TextStyle {
                        font: asset_server.load("buttons/joystix monospace.ttf"),
                        font_size: 40.0,
                        color: Color::BLACK,
                    },
                ),
                // health of player's monster
                TextSection::from_style(TextStyle {
                    font: asset_server.load("buttons/joystix monospace.ttf"),
                    font_size: 40.0,
                    color: Color::BLACK,
                }),
            ])
            .with_style(Style {
                align_self: AlignSelf::FlexEnd,
                position_type: PositionType::Absolute,
                position: UiRect {
                    top: Val::Px(5.0),
                    left: Val::Px(15.0),
                    ..default()
                },
                ..default()
            }),
        )
        .insert(MultPlayerHealth)
        .insert(MultBattleUIElement);

    commands
        .spawn_bundle(
            // Create a TextBundle that has a Text with a list of sections.
            TextBundle::from_sections([
                // health header for opponent's monster
                TextSection::new(
                    "Health:",
                    TextStyle {
                        font: asset_server.load("buttons/joystix monospace.ttf"),
                        font_size: 40.0,
                        color: Color::BLACK,
                    },
                ),
                // health of opponent's monster
                TextSection::from_style(TextStyle {
                    font: asset_server.load("buttons/joystix monospace.ttf"),
                    font_size: 40.0,
                    color: Color::BLACK,
                }),
            ])
            .with_style(Style {
                align_self: AlignSelf::FlexEnd,
                position_type: PositionType::Absolute,
                position: UiRect {
                    top: Val::Px(5.0),
                    right: Val::Px(15.0),
                    ..default()
                },
                ..default()
            }),
        )
        //.insert(MonsterBundle::default())
        .insert(MultEnemyHealth)
        .insert(MultBattleUIElement);
}

pub(crate) fn update_mult_battle_stats(
    _commands: Commands,
    _asset_server: Res<AssetServer>,
    mut set: ParamSet<(
        Query<&mut Health, With<SelectedMonster>>,
        Query<&mut Health, With<SelectedEnemyMonster>>,
    )>,
    mut player_health_text_query: Query<
        &mut Text,
        (With<MultPlayerHealth>, Without<MultEnemyHealth>),
    >,
    mut enemy_health_text_query: Query<
        &mut Text,
        (With<MultEnemyHealth>, Without<MultPlayerHealth>),
    >,
) {
    let mut my_health = 0;
    let mut enemy_health = 0;
    for my_monster in set.p0().iter_mut() {
        my_health = my_monster.health;
    }

    for mut text in &mut player_health_text_query {
        text.sections[1].value = format!("{}", my_health);
    }

    for enemy_monster in set.p1().iter_mut() {
        enemy_health = enemy_monster.health;
    }

    for mut text in &mut enemy_health_text_query {
        text.sections[1].value = format!("{}", enemy_health);
    }
}

pub(crate) fn spawn_mult_player_monster(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    cameras: Query<(&Transform, Entity), (With<MultCamera>)>,
    selected_monster_query: Query<(&Element, Entity), (With<SelectedMonster>)>,
) {
    if cameras.is_empty() {
        error!("No spawned camera...?");
        return;
    }

    if selected_monster_query.is_empty() {
        error!("No selected monster...?");
        return;
    }

    let (ct, _) = cameras.single();

    // why doesn't this update
    let (selected_type, selected_monster) = selected_monster_query.single();

    commands
        .entity(selected_monster)
        .remove_bundle::<SpriteBundle>()
        .insert_bundle(SpriteBundle {
            sprite: Sprite {
                flip_y: false, // flips our little buddy, you guessed it, in the y direction
                flip_x: true,  // guess what this does
                ..default()
            },
            texture: asset_server.load(&get_monster_sprite_for_type(*selected_type)),
            transform: Transform::from_xyz(ct.translation.x - 400., ct.translation.y - 100., 5.),
            ..default()
        })
        .insert(MultPlayerMonster)
        .insert(MultMonster);
}

pub(crate) fn spawn_mult_enemy_monster(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    cameras: Query<(&Transform, Entity), (With<MultCamera>)>,
    selected_monster_query: Query<(&Element, Entity), (With<SelectedEnemyMonster>)>,
) {
    if cameras.is_empty() {
        error!("No spawned camera...?");
        return;
    }

    if selected_monster_query.is_empty() {
        error!("No selected monster...?");
        return;
    }

    let (ct, _) = cameras.single();

    let (selected_type, selected_monster) = selected_monster_query.single();

    commands
        .entity(selected_monster)
        .remove_bundle::<SpriteBundle>()
        .insert_bundle(SpriteBundle {
            sprite: Sprite {
                flip_y: false, // flips our little buddy, you guessed it, in the y direction
                flip_x: false, // guess what this does
                ..default()
            },
            texture: asset_server.load(&get_monster_sprite_for_type(*selected_type)),
            transform: Transform::from_xyz(ct.translation.x + 400., ct.translation.y - 100., 5.),
            ..default()
        })
        .insert(MultEnemyMonster)
        .insert(MultMonster);

    commands.remove_resource::<ReadyToSpawnEnemy>();
    commands.insert_resource(EnemyMonsterSpawned {});
}

fn despawn_mult_battle(
    mut commands: Commands,
    camera_query: Query<Entity, With<MultCamera>>,
    background_query: Query<Entity, With<MultBattleBackground>>,
    monster_query: Query<Entity, With<MultMonster>>,
    mult_battle_ui_element_query: Query<Entity, With<MultBattleUIElement>>,
    selected_monster_query: Query<Entity, (With<SelectedMonster>)>,
) {
    if background_query.is_empty() {
        error!("background is not here!");
    }

    background_query.for_each(|background| {
        commands.entity(background).despawn();
    });

    monster_query.for_each(|monster| {
        commands.entity(monster).despawn_recursive();
    });

    if mult_battle_ui_element_query.is_empty() {
        error!("ui elements are not here!");
    }

    mult_battle_ui_element_query.for_each(|mult_battle_ui_element| {
        commands.entity(mult_battle_ui_element).despawn_recursive();
    });

    selected_monster_query.for_each(|monster| commands.entity(monster).despawn_recursive());
}

/// Calculate effects of the current combined turn.
///
/// # Usage
/// With explicit turn ordering, the host will take their turn first, choosing an action ID. The
/// host will then send this action number to the client and ask them to return their own action ID.
/// The client will have to send its monster stats to the host as well as the action ID in case of
/// the use of a buff which modifies strength.
/// Once the host receives this ID and the stats, it has everything it needs to call this function
/// and calculate the results of the turn, and get a tuple of the damage for both players. The
/// host can then send this tuple to the client to update their information, as well as update it
/// host-side
///
///
/// ## Action IDs
/// 0 - attack
///
/// 1 - defend
///
/// 2 - elemental
///
/// 3 - special
///
/// ## Strength Buff Modifiers
/// This function takes no information to tell it whether or not a buff is applied, and relies on the person with the
/// buff applied modifying their strength by adding the buff modifier to it and then undoing that after the turn
/// is calculated.
fn mult_calculate_turn(
    player_atk: u8,
    player_crt: u8,
    player_def: u8,
    player_type: u8,
    player_action: u8,
    enemy_atk: u8,
    enemy_crt: u8,
    enemy_def: u8,
    enemy_type: u8,
    enemy_action: u8,
    type_system: TypeSystem,
) -> (isize, isize) {
    if player_action == 1 || enemy_action == 1 {
        // if either side defends this turn will not have any damage on either side
        return (0, 0);
    }
    // More actions can be added later, we can also consider decoupling the actions from the damage
    let mut result = (
        0, // Your damage to enemy
        0, // Enemy's damage to you
    );
    // player attacks
    // If our attack is less than the enemy's defense, we do 0 damage
    if player_atk <= enemy_def {
        result.0 = 0;
    } else {
        // if we have damage, we do that much damage
        // I've only implemented crits for now, dodge and element can follow
        result.0 = (player_atk - enemy_def) as usize;
        // if player_crt > 15 {
        //     // calculate crit chance and apply crit damage
        //     let crit_chance = player_crt - 15;
        //     let crit = rand::thread_rng().gen_range(0..=100);
        //     if crit <= crit_chance {
        //         info!("You had a critical strike!");
        //         result.0 *= 2;
        //     }
        // }
    }
    // same for enemy
    if enemy_atk <= player_def {
        result.1 = 0;
    } else {
        result.1 = (enemy_atk - player_def) as usize;
        // if enemy_crt > 15 {
        //     let crit_chance = enemy_crt - 15;
        //     let crit = rand::thread_rng().gen_range(0..=100);
        //     if crit <= crit_chance {
        //         info!("Enemy had a critical strike!");
        //         result.1 *= 2;
        //     }
        // }
    }

    if player_action == 2 {
        // Elemental move
        result.0 = (type_system.type_modifier[player_type as usize][enemy_type as usize]
            * result.0 as f32)
            .trunc() as usize;
    } else if player_action == 3 {
        // Multi-move
        // Do an attack first
        result.0 += mult_calculate_turn(
            player_atk,
            player_crt,
            player_def,
            player_type,
            0,
            enemy_atk,
            enemy_crt,
            enemy_def,
            enemy_type,
            enemy_action,
            type_system,
        )
        .0 as usize;
        // Then simulate elemental
        result.0 = (type_system.type_modifier[player_type as usize][enemy_type as usize]
            * result.0 as f32)
            .trunc() as usize;
    }

    if enemy_action == 2 {
        result.1 = (type_system.type_modifier[enemy_type as usize][player_type as usize]
            * result.1 as f32)
            .trunc() as usize;
    } else if enemy_action == 3 {
        // Multi-move
        // Do an attack first
        result.1 += mult_calculate_turn(
            player_atk,
            player_crt,
            player_def,
            player_type,
            player_action,
            enemy_atk,
            enemy_crt,
            enemy_def,
            enemy_type,
            0,
            type_system,
        )
        .1 as usize;
        // Then simulate elemental
        result.1 = (type_system.type_modifier[enemy_type as usize][player_type as usize]
            * result.1 as f32)
            .trunc() as usize;
    }

    (result.0 as isize, result.1 as isize)
}
