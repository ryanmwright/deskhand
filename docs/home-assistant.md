# Using Deskhand with Home Assistant

## The broker

Deskhand talks to Home Assistant through an MQTT broker; nothing is installed in
Home Assistant itself. The usual setup:

1. **Settings → Add-ons → Mosquitto broker**: install and start it. (Any other
   MQTT 3.1.1 broker works too.)
2. **Settings → Devices & services**: Home Assistant offers the **MQTT**
   integration once the add-on runs. Add it. MQTT discovery is on by default.
3. Create a login for Deskhand. With the Mosquitto add-on, any Home Assistant
   user can log in: **Settings → People → Users → Add user**, e.g. `deskhand`
   (it doesn't need to be an administrator), or add a login under the add-on's
   **Configuration → Logins**.

If your broker uses ACLs, Deskhand needs to read and write `deskhand/<node>/#`
and to write `homeassistant/#` (see [SECURITY.md](../SECURITY.md)).

Then run `deskhand setup` on the desktop. A minute later the computer shows up
under **Settings → Devices & services → MQTT** as a device with its entities.

## Entity ids

Home Assistant builds entity ids from the device name and the entity name. For
a device called "Office PC":

| Deskhand | Home Assistant |
|---|---|
| `session_state` | `sensor.office_pc_session_state` |
| `in_call` | `binary_sensor.office_pc_in_call` |
| `media_play_pause` | `button.office_pc_play_pause` |
| `notify` | `notify.office_pc_notify` |
| `notification_event` | `event.office_pc_notification` |
| `shortcut_lights` named "Toggle desk lights" | `event.office_pc_toggle_desk_lights` |

The examples below use these; check yours on the device page. The full list is
in [entities.md](entities.md). `deskhand doctor` prints the node id, needed for
the rich notification topic.

## Automations

All of these go into **Settings → Automations → Create → Edit in YAML**, or
into `automations.yaml`. Ready-made blueprints for three of them are in
[`blueprints/`](../blueprints).

### Lights off when the computer locks or sleeps

`session_state` is the single sensor to watch: `active`, `idle`, `locked`,
`sleeping` or `off` (shut down or Deskhand stopped).

```yaml
alias: Desk lights off when the PC is locked or asleep
triggers:
  - trigger: state
    entity_id: sensor.office_pc_session_state
    to: [locked, sleeping, "off"]
actions:
  - action: light.turn_off
    target:
      entity_id: light.desk
```

And back on when you return:

```yaml
alias: Desk lights on when I'm back
triggers:
  - trigger: state
    entity_id: sensor.office_pc_session_state
    to: active
    from: [locked, idle, sleeping]
conditions:
  - condition: sun
    after: sunset
actions:
  - action: light.turn_on
    target:
      entity_id: light.desk
```

### Meeting light

`in_call` turns on when an app has used the microphone for 10 seconds, or the
camera is on, and off a few seconds after both stop.

```yaml
alias: On-air light
triggers:
  - trigger: state
    entity_id: binary_sensor.office_pc_in_call
actions:
  - if:
      - condition: state
        entity_id: binary_sensor.office_pc_in_call
        state: "on"
    then:
      - action: light.turn_on
        target:
          entity_id: light.door_sign
        data:
          color_name: red
    else:
      - action: light.turn_off
        target:
          entity_id: light.door_sign
```

### Pause media when the screen locks

```yaml
alias: Pause music when the PC locks
triggers:
  - trigger: state
    entity_id: sensor.office_pc_session_state
    to: locked
conditions:
  - condition: state
    entity_id: sensor.office_pc_media_state
    state: playing
actions:
  - action: button.press
    target:
      entity_id: button.office_pc_play_pause
```

### Notifications on the desktop

A plain message through the notify entity:

```yaml
alias: Tell the PC when the washer is done
triggers:
  - trigger: state
    entity_id: sensor.washer_state
    to: finished
actions:
  - action: notify.send_message
    target:
      entity_id: notify.office_pc_notify
    data:
      title: Washer
      message: The washing is done.
```

### Notify your phone when a job on the computer finishes

A `[[script]]` reports its outcome in a result sensor
([configuration.md](configuration.md#script-buttons-that-run-commands)). With
`[[script]] id = "backup", name = "Run backup"`:

```yaml
alias: Backup finished
triggers:
  - trigger: state
    entity_id: sensor.office_pc_run_backup_result
    from: running
actions:
  - action: notify.mobile_app_my_phone
    data:
      message: >
        Backup {{ trigger.to_state.state }}
        after {{ trigger.to_state.attributes.duration_s }} s.
```

Start it from Home Assistant with `button.press` on `button.office_pc_run_backup`.

### Actionable notifications

Rich notifications are JSON published to `deskhand/<node>/notification/set`.
Buttons and the inline reply come back as `notification_event` events;
nothing is run on the desktop.

| Key | |
|---|---|
| `title`, `message` | Text. |
| `icon` | Icon name (`dialog-information`, `security-high`) or a file path. |
| `urgency` | `low`, `normal`, `critical`. |
| `timeout` | Seconds; `0` keeps it until dismissed. |
| `tag` | A later notification with the same tag replaces it; `{"tag": "door", "dismiss": true}` closes it. Events carry the tag. |
| `actions` | Buttons: `{"id": "unlock", "label": "Unlock"}`, or a string used as both. The id `default` is clicking the notification. |
| `reply` | `{"placeholder": "Answer…", "button": "Send"}` adds a text field (Plasma). |

```yaml
alias: Doorbell on the desktop
triggers:
  - trigger: state
    entity_id: binary_sensor.doorbell
    to: "on"
actions:
  - action: mqtt.publish
    data:
      topic: deskhand/0123456789ab/notification/set   # node id from `deskhand doctor`
      payload: >
        {{ {"title": "Doorbell", "message": "Someone is at the front door",
            "urgency": "critical", "tag": "door",
            "actions": [{"id": "unlock", "label": "Unlock"}, "Ignore"]} | to_json }}
  - wait_for_trigger:
      - trigger: state
        entity_id: event.office_pc_notification
    timeout: "00:02:00"
  - if:
      - "{{ wait.trigger is not none
            and wait.trigger.to_state.attributes.tag == 'door'
            and wait.trigger.to_state.attributes.event_type == 'action'
            and wait.trigger.to_state.attributes.action == 'unlock' }}"
    then:
      - action: lock.unlock
        target:
          entity_id: lock.front_door
```

### Keyboard shortcuts that control the house

A `[[shortcut]]` (KDE) fires `pressed` on its event entity each time you press
the keys. An event entity's state is the time of the last event, so trigger on
any change except coming back from `unavailable`:

```yaml
alias: Meta+Shift+L toggles the desk lights
triggers:
  - trigger: state
    entity_id: event.office_pc_toggle_desk_lights
    not_from: [unavailable, unknown]
conditions:
  - condition: state
    entity_id: event.office_pc_toggle_desk_lights
    attribute: event_type
    state: pressed
actions:
  - action: light.toggle
    target:
      entity_id: light.desk
```

### Idle and presence

`user_active` turns off after `idle_threshold` (default 5 minutes) without
keyboard or mouse input; a playing video doesn't keep it on.

```yaml
alias: Dim the office when I'm away from the keyboard
triggers:
  - trigger: state
    entity_id: binary_sensor.office_pc_user_active
    to: "off"
    for: "00:10:00"
actions:
  - action: light.turn_on
    target:
      entity_id: light.office
    data:
      brightness_pct: 20
```

## A media player card

Deskhand publishes the media controls as separate entities, which work with
stock Home Assistant. For a single media player entity (with seeking and a
media card), install the HACS integration
[mqtt_media_player](https://github.com/bkbilly/mqtt_media_player), add
`mqtt_media_player:` to `configuration.yaml`, restart Home Assistant, and set:

```toml
[integrations.media]
compat_mqtt_media_player = true
```

## Removing a computer

`deskhand uninstall` removes everything Deskhand published for this computer
from the broker, which removes the device from Home Assistant. Stop the service
first (`systemctl --user disable --now deskhand`), or it publishes again.

## Troubleshooting

- `deskhand doctor`: config, broker login, whether the daemon is online, and
  which integration provides each capability (and why others don't).
- `journalctl --user -u deskhand -f`: the daemon's log.
- Entities stay unavailable: check `deskhand/<node>/status` with
  `mosquitto_sub -h <broker> -u <user> -P <password> -t 'deskhand/#' -v`. It
  reads `offline` while the computer sleeps, the daemon is stopped, or
  publishing is paused from the tray.
- "not authorised" from setup or doctor: wrong username or password, or the
  broker's ACL does not allow the topics above.
- An entity is missing: `deskhand doctor` lists the capability as unavailable,
  with the reason (for example, KDE integrations need a Plasma session).
