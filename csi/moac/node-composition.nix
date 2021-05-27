# This file has been generated by node2nix 1.9.0. Do not edit!

{ pkgs ? import <nixpkgs> {
    inherit system;
  }
, system ? builtins.currentSystem
, nodejs-slim ? pkgs."nodejs-slim-16_x"
, nodejs ? pkgs."nodejs-16_x"
}:

let
  nodeEnv = import ./node-env.nix {
    inherit (pkgs) stdenv lib python2 runCommand writeTextFile;
    inherit pkgs nodejs nodejs-slim;
    libtool = if pkgs.stdenv.isDarwin then pkgs.darwin.cctools else null;
  };
in
import ./node-packages.nix {
  inherit (pkgs) fetchurl nix-gitignore stdenv lib fetchgit;
  inherit nodeEnv;
}
