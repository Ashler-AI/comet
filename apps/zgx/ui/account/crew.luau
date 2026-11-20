-- Crew (docs/crew/zgx-thin-port.md, slice 4): this build signs in with
-- Scaffold through its own engine, and there's no Zeron account behind it:
-- no username, card or organization to make, no profile to edit and no
-- members to manage (src/crew.rs answers those calls itself). The host says
-- which build this is once, as the window opens (its `crew` flag): every
-- Crew difference in the scripts asks here, not the account's provider.
--
--   crew.on          whether this is a Crew build
--   crew.PRODUCT     what the app is called in one (its flavour's: the
--                    host's `crew_product`, crates/cli `crew::PRODUCT`)
--   crew.managed(id) whether a Crew build runs that agent itself

local M = {}

M.on = ui.shared("crew", false):peek() == true
M.PRODUCT = ui.shared("crew_product", "Crew (zgx dev)"):peek()

-- The agents a Crew build runs, each on Crew's shared accounts through
-- Agent Auth: the engine's managed catalogs
-- (crates/harness/src/crew_inference.rs `managed_models`, whose test reads
-- this line). Every other agent fails closed there, so setup and Settings →
-- Providers offer these alone, with nothing of their own to sign in to.
local MANAGED = { ["claude-code"] = true, ["codex"] = true }

function M.managed(id: string): boolean
	return MANAGED[id] == true
end

return M
