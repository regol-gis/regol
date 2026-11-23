(
  area[name="East Midlands"][boundary=statistical];
  area[name="West Midlands"][boundary=statistical];
)->.midlands;
node(area.midlands)["generator:source"="wind"]->.turbines;
foreach.turbines->.turbine {
  node.turbines(around.turbine:1000)->.first;
  node.turbines(around.first:1000)->.second;
  if ((first.count(nodes) == 5) && (second.count(nodes) == 5)) {
    (.groups; .first;)->.groups;
  }
}
.groups out body;
.groups out count;
