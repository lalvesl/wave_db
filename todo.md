# TO DO

- Falta de clareza sobre aquizição de dados por clients;

* I need to remove serde and postcard dependencies, i need to own procedure-macros, the objective is reduce size of wasm, in procedure macro create methods to get size_of at compile time and add with a method to get size of heap data, to request allocation of memory only once, the data is exacly the memory for stack elements and for dynamic use u32 to determinate size and in the sequence the heap data, to parse data the object need to be knowed by bolf parts, think this when i create objects with macro of wave_db create space of declaration of all objects, this are exposed by all nodes and can searchable by header u32(u24 of struct_id and u8 with the version of data), the implementation use another procedure macro to generate code for each version and expose a module for specific struct_id to need declared all to start quick,slow and client nodes, with this method is extreme more easy to access heap properties(such as a current list of names of heap props), know what properties and how to organize data for Anchors indexes, NonUnique and also NestedNonUnique, and also reduce usage of dyn traits because all cases are compiled statically, yes in the future there is possible to share cfg conde between clients, quick and slow nodes like nextjs but not only client/server because the DB are server also;

* Remove serde,postcard crates from this repository, create own implementation of serde

* In query there's an implementation of enum @crates/wavedb/src/query.rs#L39-53 to describe data to quering, add all types of number f|u|i/8|16|32|64|128;

# DOING

# DONE

- read the @readme.md and undestand this project. There is a problem with expressions, i need to write the name of column in str, i want to replace this with enum os each column. Take as much time as you need!
- The description on @readme.md#L17-28 is not describe correcly this project, read again the @readme.md and describe the problems of common sql, mixing data of all users, and mixing data of elements not reletead (the NestedNonUnique) when data are storege and searcheable only with interested data, reducing cache and diskIOps;
