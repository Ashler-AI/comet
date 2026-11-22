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
--   crew.sandbox_host(id)  whether that device is one of its sandboxes' hosts

local M = {}

M.on = ui.shared("crew", false):peek() == true
M.PRODUCT = ui.shared("crew_product", "Crew (zgx dev)"):peek()

-- Agents run on Crew's shared accounts through Agent Auth, unless the app
-- was launched with the development switch (`COMET_INFERENCE_MODE=local`:
-- the host's `crew_managed`, src/crew.rs `local_inference`), when they run
-- on this device's own logins, as Zeron's do.
M.managed_inference = M.on and ui.shared("crew_managed", true):peek() ~= false

-- The agents a Crew engine runs on Agent Auth
-- (crates/harness/src/crew_inference.rs `Provider::managed_harness`, whose
-- test reads this line); each shows once the engine lists it. Every other
-- agent fails closed there, so setup and Settings → Providers offer these
-- alone, with nothing of their own to sign in to.
local MANAGED = { ["claude-code"] = true, ["codex"] = true, ["omp"] = true }

function M.managed(id: string): boolean
	return M.managed_inference and MANAGED[id] == true
end

function M.hides(id: string): boolean
	return M.managed_inference and MANAGED[id] ~= true
end

-- The Cloud row's sandboxes each run as a host of their own,
-- `comet-scaffold-{sandbox}-e{epoch}` (crates/engine/src/crew_scaffold.rs
-- `device_id_for`). A chat on one follows its sandbox as a Cloud chat follows
-- its machine (screens/chat/cloud_tray): starting, or why it failed. It isn't
-- one of Cloud's devices otherwise (its accounts are its own, for one).
local SANDBOX_HOST = "comet-scaffold-"

function M.sandbox_host(device: string?): boolean
	return M.on and device ~= nil and string.sub(device, 1, #SANDBOX_HOST) == SANDBOX_HOST
end

return M
