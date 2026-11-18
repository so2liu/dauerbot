# Project model selection

Open a project's model control in Dauerbot, or send `/model` or `/models` to
its Feishu bot. The bot replies in that message's topic with an interactive
picker. Choose a discovered native model, enter an exact model ID, or select
native CLI/session settings. Pi and Codex also accept an existing provider ID.
Following native settings removes the project's launch override; a resumed
session may retain its saved model. Choose an explicit model to require one.

Preferences belong to the project and CLI. They do not rewrite global CLI
settings, replace credentials, sign the user in, or add model providers.
The selected model and a model confirmed by the running CLI are distinct.
A cached catalogue or a successful startup alone does not prove the model used
by a turn. A CLI may still reject an unknown model or a model unavailable to
the signed-in account.

The card can be used once, by the user who requested it, in its original
conversation. It expires after 30 minutes. Open a new card after changing the
project's CLI or native session. A forwarded card cannot change another
project. Normal Feishu instructions keep using the Agent delivery path.

## Feishu callback configuration

In the Feishu developer console, open the project's existing bot application.
Under **Events & Callbacks → Callback configuration**, enable the card action
trigger callback, `card.action.trigger`, using the existing long connection.
Publish the application changes where required by the tenant. Dauerbot uses
the bot's existing WebSocket; no public callback URL or second listener is
required. Sending ordinary messages does not prove callbacks are enabled.

If the card renders but submitting it does nothing, check callback
configuration and application publication. During an update a click may
receive a visible “not accepted” response; retry that click after the update.
Accepted callbacks are stored before acknowledgement and reuse the same
request identity after a restart.

## Optional bot-menu shortcut

In **Bot → Bot menu**, add a menu item with the name `/models` and choose
**Send message** (`send_message`) as its behavior. The menu sends its **name**
as user text, so the name must be `/models`; a friendly label such as “Models”
does not invoke this command. Publish the bot-menu change where required.
The same `/models` message opens the picker in a chat; no menu-specific event
handler is needed.

This is a setup guide. Dauerbot does not silently change an application's
existing menu or its developer-console settings.
