{
  description = "Logos Uniswap module — V2/V3/V4 price oracle (best-rate, Multicall3) + V2/V3 swap building. Multi-chain, configurable.";

  inputs = {
    logos-module-builder.url = "github:logos-co/logos-module-builder";

    # Dependency module. Its published `.lidl` contract drives the generated
    # `modules().eth_rpc_module` typed client used to issue the batched eth_call.
    # The `follows` makes it use the SAME module-builder as this module.
    eth_rpc_module = {
      url = "github:logos-co/logos-evm-eth-rpc-module";
      inputs.logos-module-builder.follows = "logos-module-builder";
    };
  };

  outputs = inputs@{ self, logos-module-builder, ... }:
    let
      nixpkgs = logos-module-builder.inputs.nixpkgs;
      systems = [ "aarch64-darwin" "x86_64-darwin" "aarch64-linux" "x86_64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems f;
    in
    {
      packages = forAllSystems (system:
        (logos-module-builder.lib.mkLogosModule {
          src = ./.;
          configFile = ./metadata.json;
          flakeInputs = inputs;
        }).packages.${system});
    };
}
