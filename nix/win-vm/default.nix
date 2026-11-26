{
  pkgs,
  assetOverrides ? { },
  imageOverrides ? { },
}:
let
  assets = pkgs.callPackage ./assets.nix assetOverrides;
  image = pkgs.callPackage ./image.nix ({ inherit assets; } // imageOverrides);
  runner = pkgs.callPackage ./runner.nix { baseImage = image; };
  lazyRunner = pkgs.callPackage ./runner.nix { baseImage = null; };
in
{
  inherit
    assets
    image
    runner
    lazyRunner
    ;
}
