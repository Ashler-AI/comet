-- Crew (docs/crew/zgx-thin-port.md, slice 4): this build signs in with
-- Scaffold through its own engine, and there's no Zeron account behind it:
-- no username, card or organization to make, no profile to edit and no
-- members to manage (src/crew.rs answers those calls itself). The host says
-- which build this is once, as the window opens (its `crew` flag): every
-- Crew difference in the scripts asks here, not the account's provider.
--
--   crew.on        whether this is a Crew build
--   crew.PRODUCT   what the app is called in one

local M = {}

M.on = ui.shared("crew", false):peek() == true
M.PRODUCT = "Crew (zgx dev)"

return M
