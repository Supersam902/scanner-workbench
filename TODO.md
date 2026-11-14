# Multiple adapter support

- [x] Separate diagnostic requests from ELM adapter commands.
- [x] Add native TCP connections for ELM-compatible network adapters.
- [x] Add J2534 04.04 driver discovery and ISO 15765 OBD connections.
- [x] Expose connection selection in the desktop application.
- [x] Test shared operations through simulated serial/TCP and J2534 interfaces.
- [x] Document supported connections and hardware validation limits.

Later adapter work:

- [ ] Validate protocol selection and connections with more physical adapters.
- [ ] Evaluate standard OBD transport over CAN FD and DoIP where supported by drivers.
- [ ] Validate real adapters and vehicles; simulation does not establish vehicle coverage.
