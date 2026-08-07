#!/usr/bin/env ruby

require "json"
require "yaml"

source, destination = ARGV
abort "usage: generate_registers.rb sensor.yaml registers.json" unless source && destination

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
    current = registers[address.to_s]
    registers[address.to_s] = candidate if current.nil? || candidate["width"] < current["width"]
  end
  pages[page.fetch("page_id").to_i.to_s] = registers
end

payload = {
  "sensor" => chip.fetch("sensor"),
  "source" => File.basename(source),
  "pages" => pages
}
File.write(destination, JSON.pretty_generate(payload) + "\n")
