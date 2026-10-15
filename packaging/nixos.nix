{ config, lib, pkgs, ... }:
let
  cfg = config.services.app-daemon.resources;
in
{
  options.services.app-daemon.resources.enableRaplAccess = lib.mkEnableOption "read-only video-group access to CPU package energy counters";
  config = lib.mkIf cfg.enableRaplAccess {
    # Reading counters does not require enabling power limits. Several AMD
    # implementations expose enabled but reject writes with ENOSYS.
    services.udev.extraRules = ''
      ACTION=="add|change", SUBSYSTEM=="powercap", TEST=="energy_uj", RUN+="${pkgs.coreutils}/bin/chgrp video /sys%p/energy_uj", RUN+="${pkgs.coreutils}/bin/chmod 0440 /sys%p/energy_uj"
    '';
  };
}
