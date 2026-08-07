#!/usr/bin/env ruby

require "json"
require "yaml"

source, destination = ARGV
abort "usage: generate_registers.rb qmi8660.yaml qmi8660_registers.json" unless source && destination

chip = YAML.load_file(source)
pages = {}
chip.fetch("pages").each_value do |page|
  registers = {}
  page.fetch("registers").each do |register|
    address = register.fetch("addr").to_i
    candidate = {
      "name" => register.fetch("name"),
      "access" => register.fetch("access"),
      "width" => register.fetch("width", 1),
      "description" => register.fetch("desc", ""),
      "roles" => register.fetch("roles", []),
      "fields" => register.fetch("fields", []).map do |field|
        {
          "name" => field.fetch("name"),
          "bits" => field.fetch("bits"),
          "description" => field.fetch("desc", ""),
          "roles" => field.fetch("roles", []),
          "event" => field["event"],
          "ignore_by_default" => field.fetch("ignore_by_default", false)
        }
      end
    }
    # Duplicate addresses in the source are aliases. The single-byte view is
    # the useful definition for normal burst field decoding.
    current = registers[address.to_s]
    registers[address.to_s] = candidate if current.nil? || candidate["width"] < current["width"]
  end
  pages[page.fetch("page_id").to_i.to_s] = registers
end

payload = {
  "sensor" => chip.fetch("sensor"),
  "source" => "rseq/qmi8660.yaml",
  "pages" => pages
}
File.write(destination, JSON.pretty_generate(payload) + "\n")
