-- Setup's Sign in (screens/onboarding/signin), mounted on its own in
-- setup's frame, its ways on recorded instead of taken: src/crew_tests.rs
-- mounts it with and without the host's `crew` flag and reads
-- `crew_signin_went` (the last way taken: "create", "existing", "cancel" or
-- "skip"); its `crew_signin.waiting` command puts it to waiting on the
-- browser, as a sign-in under way does.

ui.theme.strict("warn")
local ds = require("@ds")
ds.setup()
local signin = require("@screens/onboarding/signin")

local status = ui.state("crew_signin_status", "ready")
local went = ui.state("crew_signin_went", "")
local function way(name: string): () -> ()
	return function()
		went:set(name)
	end
end

ui.command {
	id = "crew_signin.waiting",
	title = "Wait on the browser",
	run = function()
		status:set("waiting")
	end,
}

return ds.Onboarding.Frame {
	signin.screen {
		status = status,
		on_create = way("create"),
		on_existing = way("existing"),
		on_cancel = way("cancel"),
		on_skip = way("skip"),
	},
}
