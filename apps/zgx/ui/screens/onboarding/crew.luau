-- Crew's one way through setup (ui/account/crew.luau): signed in with
-- Scaffold, the look, the agents, done. A Crew account has no username or
-- card to claim, no organization to name and nobody to invite from here: it
-- works in the project the engine signed into. (Without an account, the way
-- is Zeron's own: the look and the agents.)

local crew = require("@account/crew")

local M = {}

M.on = crew.on
M.PATH = { "signin", "appearance", "providers", "done" }

return M
