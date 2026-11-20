-- Settings → Account in a Crew build (ui/account/crew.luau), in Profile's
-- place: its id is `profile`, so the sidebar foot's you opens it and
-- `settings/account` lands here. Whom you're signed in as, with Scaffold,
-- and the project your sessions work in; the computers they sync across;
-- and signing out (out of plugins' reach: it ends the engine's session and
-- opens setup again). Nothing of a Zeron account to edit: no name, title or
-- photo, no username or card, and no deleting the account from here. No
-- signing keys either: a Crew engine keeps no plugin-key directory, since
-- Crew's Edge has none (crates/engine/src/lib.rs).

local host = require("@app/screens/settings/host")
local ds = require("@ds")
local auth = require("@account/auth")
local sessions = require("@account/sessions")
local store = require("@app/screens/settings/store")
local search = require("@app/screens/settings/search")
local device_icon = require("@app/screens/settings/devices").icon

local M = {}

M.ID = "profile"
M.TITLE = "Account"

-- (Profile's rows, less what a Crew account can't change: this page's index
-- in its place.)
search.index(M.ID, ui.t("settings.profile.title"), ui.t("settings.profile.terms"), {
	{ key = "email", title = ui.t("settings.profile.email.label"), terms = ui.t("settings.profile.email.terms") },
	{ key = "devices", title = ui.t("settings.profile.devices.label"), terms = ui.t("settings.profile.devices.terms") },
	{ key = "sign_out", title = ui.t("settings.profile.sign_out.label"), terms = ui.t("settings.profile.sign_out.terms") },
})

-- Signing out, once asked (the row holds still while it goes).
local leaving = ui.state("settings_crew_signing_out", false)
local confirming = ui.state("settings_crew_sign_out_confirm", false)
local function sign_out()
	if leaving:peek() then
		return
	end
	leaving:set(true)
	auth.sign_out(function()
		store.forget()
		leaving:set(false)
		-- (The shell may ask too, as it sees this window signed out: the host
		-- opens setup once a window.)
		host.invoke("app.onboard")
	end)
end

-- The project the session works in (Scaffold's scope), as the engine says.
local function project(): string?
	local a = auth.account:get() :: any
	return a.organization_id
end

local function signed_out(): boolean
	return not auth.signed_in:get()
end

function M.view(): Node
	-- Where you're signed in: your devices, not Cloud sessions' machines.
	local devices = ui.computed(function(): { sessions.Device }
		return sessions.computers(sessions.devices:get())
	end)
	return ui.column {
		w_full = true,
		items = "center",
		ds.Settings.Page {
			title = ui.t("settings.profile.title"),
			-- Whom you're signed in as, and where your sessions sync.
			search.anchor(
				"email",
				ds.Settings.Section {
					title = ui.t("settings.profile.email.label"),
					ds.Settings.Group {
						ds.Settings.Row {
							label = function(): string
								local u = auth.user:get()
								return if u then u.email else ui.t("settings.profile.email.not_signed_in"):get()
							end,
							private_label = true,
							description = function(): string
								if signed_out() then
									return ui.t("settings.profile.email.local"):get()
								end
								local p = project()
								return if p
									then ui.t("settings.profile.email.org", { org = p }):get()
									else ui.t("settings.profile.email.sync"):get()
							end,
							icon = "user-rounded",
							truncate = true,
						},
					},
				}
			),
			ui.show(
				auth.signed_in,
				search.anchor(
					"devices",
					ds.Settings.Section {
						title = ui.t("settings.profile.devices.label"),
						ds.Settings.Each(devices, function(_d: sessions.Device, _i: number, d: State<sessions.Device>): Node
							return ds.Settings.Row {
								label = function()
									return d:get().name
								end,
								description = function()
									local p = d:get().platform
									return if p == "macos" then "macOS" elseif p == "linux" then "Linux" elseif p == "windows" then "Windows" elseif p == "ios" then "iOS" elseif p == "android" then "Android" else p
								end,
								icon = device_icon(d:peek().platform),
								badge = if d:peek().here then ds.Badge { ui.t("settings.profile.devices.this") } else nil,
							}
						end, {
							key = function(d: sessions.Device): string
								return d.id
							end,
						}),
					}
				)
			),
			-- Signing out, last: out of plugins' reach. Signed out, the way in
			-- (setup, as signing out leaves you).
			ui.column {
				hook = false,
				w_full = true,
				search.anchor(
					"sign_out",
					ds.Settings.Section {
						ds.Settings.Group {
							-- (One child: the Group rules between its children.)
							ui.column {
								w_full = true,
								ui.show(
									auth.signed_in,
									ds.Settings.Row {
										label = function(): string
											return if leaving:get()
												then ui.t("settings.profile.sign_out.busy"):get()
												else ui.t("settings.profile.sign_out.label"):get()
										end,
										description = ui.t("settings.profile.sign_out.description"),
										danger = true,
										on_click = function()
											if not leaving:peek() then
												confirming:set(true)
											end
										end,
									}
								),
								ui.show(
									signed_out,
									ds.Settings.Row {
										label = ui.t("settings.profile.signed_out.sign_in"),
										icon = "user-rounded",
										on_click = function()
											host.invoke("app.onboard")
										end,
									}
								),
							},
						},
					}
				),
			},
		},
		ds.Dialog {
			open = confirming,
			title = ui.t("crew.account.sign_out_title"),
			body = ui.t("settings.profile.sign_out.confirm_body"),
			tone = "danger",
			confirm = { label = ui.t("settings.profile.sign_out.label"), on_click = sign_out },
		},
	}
end

return M
