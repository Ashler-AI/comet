-- Crew (docs/crew/zgx-thin-port.md, slice 4): this build signs in with
-- Scaffold through its own engine, and there's no Zeron account behind it:
-- no username, card or organization to make, no profile to edit and no
-- members to manage (src/crew.rs answers those calls itself). The host says
-- which build this is once, as the window opens (its `crew` flag): every
-- Crew difference in the scripts asks here, not the account's provider.
--
--   crew.on                whether this is a Crew build
--   crew.PRODUCT           what the app is called in one (its flavour's: the
--                          host's `crew_product`, crates/cli `crew::PRODUCT`)
--   crew.managed_inference whether its agents run on Crew's shared accounts
--   crew.managed(id)       whether that agent does, signed in by Crew
--   crew.hides(id)         whether that agent isn't offered at all

local M = {}

M.on = ui.shared("crew", false):peek() == true
M.PRODUCT = ui.shared("crew_product", "Crew (zgx dev)"):peek()

-- Agents run on Crew's shared accounts through Agent Auth, unless the app
-- was launched with the development switch (`COMET_INFERENCE_MODE=local`:
-- the host's `crew_managed`, src/crew.rs `local_inference`), when they run
-- on this device's own logins, as Zeron's do.
M.managed_inference = M.on and ui.shared("crew_managed", true):peek() ~= false

-- The agents a Crew engine runs on Agent Auth: those with a managed route
-- (crates/harness/src/crew_inference.rs `Provider::for_harness`, whose test
-- reads this line). Every other agent fails closed there, so setup and
-- Settings → Providers offer these alone, with nothing of their own to sign
-- in to.
local MANAGED = { ["claude-code"] = true, ["codex"] = true }

function M.managed(id: string): boolean
	return M.managed_inference and MANAGED[id] == true
end

function M.hides(id: string): boolean
	return M.managed_inference and MANAGED[id] ~= true
end

return M
