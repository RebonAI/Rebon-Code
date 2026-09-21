# Rebon Browser Extension — Privacy Policy

_Last updated: 2026-07-28_

## Summary

The Rebon Browser extension lets the Rebon application running **on your own computer** observe and control a dedicated browser tab group. The extension itself communicates only with that local Rebon application and never contacts the developer or any server of its own. Be aware, however, that content the extension captures becomes part of your Rebon agent conversation: if you have configured Rebon to use a cloud AI model provider (for example Anthropic or OpenAI), Rebon may include that content in requests it sends to that provider. See "Where captured data can go" below.

## What the extension does with data

- All of the extension's own communication happens exclusively between the extension and the Rebon application on the same machine, over a loopback WebSocket connection (`127.0.0.1`). The extension itself sends no data to any remote server.
- Page content, screenshots, and tab information are read **only** from tabs inside the dedicated "Rebon" tab group that the extension itself created, and only while you have an active, explicitly paired Rebon session.
- The extension has no analytics, no telemetry, and no advertising.

## Where captured data can go

- Page content, screenshots, and tab information captured by the extension are delivered to the local Rebon application as tool results in your agent conversation.
- If you have configured Rebon with a cloud AI model provider (such as Anthropic, OpenAI, or another provider you set up), Rebon sends conversation content — which can include captured page content and screenshots — to that provider's servers as part of its model requests. That data is then processed by the provider under the provider's own privacy policy and terms.
- If you configure Rebon to use only a local model, captured content stays on your machine.
- The developer of the extension and of Rebon does not receive, collect, sell, or share any of this data. Which third-party provider (if any) receives conversation content is entirely determined by your own Rebon configuration.

## What is stored locally

- A pairing token (established by you entering a six-digit code shown in Rebon) and the local bridge port number are stored in the browser's extension storage (`chrome.storage.local`). They never leave your device and are removed when you remove the extension.

## Permissions

- `debugger`, `tabs`, `tabGroups`, `scripting`, `webNavigation`: used solely to create and control the dedicated Rebon tab group (navigation, clicks, typing, screenshots, and the on-page control indicator).
- `alarms`: keeps the connection to the local Rebon application alive.
- `storage`: stores the pairing token and bridge port locally.
- Optional host access (`http://*/*`, `https://*/*`): required so controlled tabs in the Rebon group can be observed and operated. Granted by you and revocable at any time.

## Data collection disclosure

The developer does not collect any user data. The extension transmits data only to the Rebon application on your own machine. Downstream, the Rebon application may transmit captured content to the AI model provider you have configured, as described in "Where captured data can go".

## Contact

Questions about this policy: bonopengate@gmail.com
