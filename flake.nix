{
  description = "vedavid-connector development environment";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "aarch64-darwin"
        "x86_64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = [
            # required for tonic-prost-build
            pkgs.protobuf
            pkgs.prometheus
          ];

          env = {
            VEDAVID_RELAY_ADDR = "127.0.0.1:8443";
            # The dev relay's certificate carries DNS:relay.localhost, so the
            # address cannot double as the name to verify.
            VEDAVID_RELAY_SERVER_NAME = "relay.localhost";
            VEDAVID_RELAY_CA = "/tmp/vedavid-dev/pki/ca.pem";
            VEDAVID_ENROLMENT_TOKEN_FILE = "/tmp/vedavid-dev/enrolment-token";
            VEDAVID_PROMETHEUS_URL = "http://127.0.0.1:9090";
          };
        };
      });
    };
}
